//! The coding panels on screen. Each reads its data from `State::code` and its cursor from the
//! open [`Panel`]; none sets a background.

use marlowe_proto::{RuleAction, SettingsScope};
use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, panel, panel_hints, row_marker};
use super::{ink, put, put_row};
use crate::panel::{
    action_label, config_rows_for, rules_ordered, scope_label, value_text, ConfigUi, DiffUi, McpUi, Panel, PermUi, QuestionUi,
    RewindUi, SettingKind, REWIND_SCOPES,
};
use crate::state::{Entry, State, Target};
use crate::text;

/// Draws the open panel. Returns where the cursor goes while a field is being typed.
pub fn draw(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let p = state.code.panel.as_ref()?;
    match p {
        Panel::Help(sc) => help(area, state, sc, theme, buf),
        Panel::Config(c) => return config(area, state, c, theme, buf),
        Panel::Permissions(p) => return permissions(area, state, p, theme, buf),
        Panel::Hooks { sel } => hooks(area, state, *sel, theme, buf),
        Panel::Agents { sel, open } => agents(area, state, *sel, *open, theme, buf),
        Panel::Mcp(m) => return mcp(area, state, m, theme, buf),
        Panel::Instructions { sel } => instructions(area, state, *sel, theme, buf),
        Panel::Context => context(area, state, theme, buf),
        Panel::Cost => cost(area, state, theme, buf),
        Panel::Rewind(r) => rewind(area, state, r, theme, buf),
        Panel::Diff(d) => diff(area, d, theme, buf),
        Panel::Jobs { sel } => jobs(area, state, *sel, theme, buf),
        Panel::Question(q) => return question(area, q, theme, buf),
        Panel::Transcript(sc) => super::transcript::draw(area, state, sc, theme, buf),
        Panel::Tasks(u) => super::bg::tasks(area, state, u, theme, buf),
        Panel::Workflows(u) => super::bg::workflows(area, state, u, theme, buf),
        Panel::Findings(u) => super::bg::findings(area, state, u, theme, buf),
        Panel::Files { sel } => super::bg::files(area, state, *sel, theme, buf),
        Panel::Plan(u) => return super::bg::plan(area, state, u, theme, buf),
        Panel::Bench(u) => super::bg::bench(area, state, u, theme, buf),
        Panel::Suggestions(u) => super::bg::suggestions(area, state, u, theme, buf),
        Panel::Thread(u) => return super::bg::thread(area, state, u, theme, buf),
        Panel::Report(u) => super::projects_report::draw(area, state, u, theme, buf),
        Panel::Call(u) => super::transcript::call(area, state, u, theme, buf),
    }
    None
}

/// Rows `first..` of an `n`-row list that keep `sel` on screen in `rows` lines.
pub(super) fn first_row(sel: usize, rows: usize) -> usize {
    sel.saturating_sub(rows.saturating_sub(1))
}

/// A centred panel: its inner area, with the hints on the bottom border.
pub(super) fn frame(area: Rect, w: u16, h: u16, title: &str, hints: &[(&str, &str)], theme: &Theme, buf: &mut Buffer) -> Rect {
    let r = centred(area, w, h, true);
    let inner = panel(r, title, ink(Ink::Accent, theme), false, buf);
    panel_hints(r, hints, theme, buf);
    inner
}

pub(super) fn note(inner: Rect, y: u16, s: &str, theme: &Theme, buf: &mut Buffer) {
    put(buf, inner.x, y, &Line::from(Span::styled(text::truncate(&text::clean(s), inner.width as usize), ink(Ink::Dim, theme))), inner.width);
}

pub(super) fn unavailable(state: &State, op: &str) -> bool {
    state.code.unavailable.contains(op)
}

fn scope_ink(s: SettingsScope) -> Ink {
    match s {
        SettingsScope::User => Ink::Dim,
        SettingsScope::Project => Ink::Speech,
        SettingsScope::Local => Ink::Accent,
        SettingsScope::Session => Ink::Dimmer,
    }
}

// ---- /help -----------------------------------------------------------------------------------------

const KEYS: &[(&str, &str)] = &[
    ("enter", "send · shift+enter or ctrl+j for a new line"),
    ("esc", "interrupt the turn · or move between the boxes"),
    ("shift+tab", "permission mode: default, accept edits, plan"),
    ("alt+t", "thinking effort, next level"),
    ("up / down", "earlier prompts, at the edge of the box · ctrl+r searches them"),
    ("ctrl+o", "transcript: every tool call in full"),
    ("ctrl+b", "move the running command to the background"),
    ("@", "mention a file · an image becomes an attachment"),
    ("!", "run a command yourself; its output goes into the context"),
    ("ctrl+v / alt+p", "paste an image from the clipboard"),
    ("/", "commands"),
    ("ctrl+k", "palette"),
    ("ctrl+n  alt+s", "new session · earlier sessions"),
    ("alt+r  ctrl+w  ctrl+g", "crew · watch · memory"),
    ("ctrl+l  ctrl+d", "library · dispatch a crew run"),
    ("ctrl+t  alt+v", "show thinking · voice"),
    ("ctrl+q", "quit: stop the agent, or close the window and keep it running"),
];

fn help(area: Rect, state: &State, sc: &crate::panel::Scroll, theme: &Theme, buf: &mut Buffer) {
    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled("Keys", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD)))];
    for (k, d) in KEYS {
        lines.push(Line::from(vec![Span::styled(text::pad(k, 24), theme.normal()), Span::styled(*d, ink(Ink::Dim, theme))]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Commands", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))));
    for b in crate::slash::BUILTINS {
        let hint = match b.args {
            crate::slash::Args::None => String::new(),
            crate::slash::Args::Optional(h) => format!(" [{h}]"),
            crate::slash::Args::Required(h) => format!(" <{h}>"),
        };
        lines.push(Line::from(vec![
            Span::styled(text::pad(&format!("/{}{hint}", b.name), 30), theme.normal()),
            Span::styled(b.help, ink(Ink::Dim, theme)),
        ]));
    }
    // Commands the daemon lists (project commands, skills, built-in prompts) and saved workflows.
    let daemon: Vec<&marlowe_proto::CommandInfo> =
        state.code.commands.iter().filter(|c| crate::slash::builtin(c.name.trim_start_matches('/')).is_none()).collect();
    if !daemon.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("From this project and your profile", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))));
        for c in daemon {
            let hint = c.argument_hint.as_deref().map(|h| format!(" {h}")).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(text::pad(&text::truncate(&format!("/{}{hint}", c.name.trim_start_matches('/')), 29), 30), theme.normal()),
                Span::styled(text::truncate(&text::clean(&c.description), 58), ink(Ink::Dim, theme)),
            ]));
        }
    }
    if !state.bg.saved.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Saved team tasks \u{b7} /team <name>", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))));
        for w in &state.bg.saved {
            lines.push(Line::from(vec![
                Span::styled(text::pad(&text::truncate(&format!("/team {}", text::clean(&w.name)), 29), 30), theme.normal()),
                Span::styled(text::truncate(&text::clean(&w.description), 58), ink(Ink::Dim, theme)),
            ]));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Commands from .marlowe/commands, .claude/commands and skills appear after / as you type.",
        ink(Ink::Dimmer, theme),
    )));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let inner = frame(area, 92, h, "help", &[("↑↓", "scroll"), ("esc", "close")], theme, buf);
    let rows = inner.height as usize;
    sc.max.set(lines.len().saturating_sub(rows));
    let first = sc.at();
    for (i, l) in lines.iter().skip(first).take(rows).enumerate() {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
}

// ---- /config ---------------------------------------------------------------------------------------

fn config(area: Rect, state: &State, c: &ConfigUi, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let rows = config_rows_for(state);
    let h = (rows.len() as u16 * 2 + 5).min(area.height.saturating_sub(2)).max(9);
    let hints: &[(&str, &str)] =
        if c.edit.is_some() { &[("↵", "save"), ("esc", "cancel")] } else { &[("↵", "change"), ("x", "unset"), ("tab", "layer"), ("esc", "close")] };
    let inner = frame(area, 96, h, "config", hints, theme, buf);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    put(
        buf,
        inner.x,
        inner.y,
        &Line::from(vec![
            Span::styled("changes are written to the ", dim),
            Span::styled(scope_label(c.scope), ink(scope_ink(c.scope), theme).add_modifier(Modifier::BOLD)),
            Span::styled(" settings", dim),
        ]),
        inner.width,
    );
    if state.code.settings.is_none() {
        let msg = if unavailable(state, "get_settings") { "this daemon does not serve settings yet" } else { "loading…" };
        note(inner, inner.y + 1, msg, theme, buf);
    }
    let per = 2u16;
    let visible = (inner.height.saturating_sub(2) / per) as usize;
    let first = first_row(c.sel, visible);
    let mut cursor = None;
    for (i, r) in rows.iter().enumerate().skip(first).take(visible) {
        let y = inner.y + 2 + (i - first) as u16 * per;
        let sel = i == c.sel;
        let value = match &r.value {
            Some(v) if r.kind == SettingKind::Bool => (if v.as_bool().unwrap_or(false) { "on" } else { "off" }).to_string(),
            Some(v) => text::clean(&value_text(v)),
            None => "not set".to_string(),
        };
        let vstyle = match (&r.value, sel) {
            (None, _) => dimmer,
            (Some(_), true) => theme.bright(),
            (Some(_), false) => theme.normal(),
        };
        let mut spans = vec![
            row_marker(sel, false, theme),
            Span::styled(text::pad(&r.key, 28), if sel { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
        ];
        let room = (inner.width as usize).saturating_sub(34 + 10);
        match (&c.edit, sel) {
            (Some(e), true) => {
                let before = &e.text()[..e.cursor()];
                cursor = Some((inner.x + 30 + text::width(&text::clean(before)).min(room) as u16, y));
                spans.push(Span::styled(text::truncate(&text::clean(e.text()), room), theme.bright()));
            }
            _ => spans.push(Span::styled(text::truncate(&value, room), vstyle)),
        }
        if let Some(src) = r.source {
            let tag = scope_label(src);
            let used = Line::from(spans.clone()).width();
            let gap = (inner.width as usize).saturating_sub(used + text::width(tag) + 1);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(tag, ink(scope_ink(src), theme)));
        } else if r.daemon {
            let used = Line::from(spans.clone()).width();
            let gap = (inner.width as usize).saturating_sub(used + 7);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled("daemon", ink(Ink::Dimmer, theme)));
        }
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
        let help = match (&c.error, sel) {
            (Some(e), true) => Span::styled(text::clean(e), ink(Ink::Amber, theme)),
            _ => Span::styled(text::truncate(&r.help, inner.width as usize - 4), dimmer),
        };
        put(buf, inner.x + 2, y + 1, &Line::from(help), inner.width.saturating_sub(2));
    }
    cursor
}

// ---- /permissions ----------------------------------------------------------------------------------

fn action_ink(a: RuleAction) -> Ink {
    match a {
        RuleAction::Allow => Ink::Green,
        RuleAction::Ask => Ink::Amber,
        RuleAction::Deny => Ink::Red,
    }
}

fn permissions(area: Rect, state: &State, p: &PermUi, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let n = state.code.rules.as_ref().map_or(0, Vec::len);
    let h = (n as u16 + 8).min(area.height.saturating_sub(2)).max(10);
    let hints: &[(&str, &str)] = if p.add.is_some() {
        &[("↵", "add"), ("←→", "allow/ask/deny"), ("tab", "layer"), ("esc", "cancel")]
    } else {
        &[("a", "add"), ("d", "remove"), ("↑↓", "select"), ("esc", "close")]
    };
    let inner = frame(area, 100, h, "permissions", hints, theme, buf);
    let dim = ink(Ink::Dim, theme);
    put(buf, inner.x, inner.y, &Line::from(Span::styled("deny beats ask beats allow · the first match in that order decides", dim)), inner.width);
    let Some(rules) = &state.code.rules else {
        let msg = if unavailable(state, "list_rules") { "this daemon does not serve permission rules yet" } else { "loading…" };
        note(inner, inner.y + 2, msg, theme, buf);
        return None;
    };
    if rules.is_empty() {
        note(inner, inner.y + 2, "no rules — everything follows the permission mode", theme, buf);
    }
    let reserve = if p.add.is_some() { 4 } else { 0 };
    let visible = (inner.height as usize).saturating_sub(2 + reserve);
    let first = first_row(p.sel, visible);
    for (i, r) in rules_ordered(rules).into_iter().enumerate().skip(first).take(visible) {
        let sel = i == p.sel;
        let y = inner.y + 2 + (i - first) as u16;
        let src = r.source.as_deref().map(text::clean).unwrap_or_default();
        let mut spans = vec![
            row_marker(sel, false, theme),
            Span::styled(text::pad(action_label(r.action), 6), ink(action_ink(r.action), theme)),
            Span::styled(text::pad(&text::clean(&r.pattern), 44), if sel { theme.bright() } else { theme.normal() }),
            Span::styled(text::pad(scope_label(r.scope), 9), ink(scope_ink(r.scope), theme)),
        ];
        let mut room = (inner.width as usize).saturating_sub(2 + 6 + 44 + 9);
        // The rule the daemon last said decided a call.
        if let Some(fired) = state.code.rules_fired.get(&r.pattern) {
            let tag = format!("fired {} ", text::truncate(&text::clean(fired), 24));
            room = room.saturating_sub(text::width(&tag));
            spans.push(Span::styled(tag, ink(Ink::Accent, theme)));
        }
        spans.push(Span::styled(text::truncate_left(&src, room), ink(Ink::Dimmer, theme)));
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
    let mut cursor = None;
    if let Some(a) = &p.add {
        let y = inner.bottom().saturating_sub(3);
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![
                Span::styled("new rule  ", dim),
                Span::styled(action_label(a.action), ink(action_ink(a.action), theme).add_modifier(Modifier::BOLD)),
                Span::styled(" in ", dim),
                Span::styled(scope_label(a.scope), ink(scope_ink(a.scope), theme)),
            ]),
            inner.width,
        );
        let shown = if a.pattern.text().is_empty() { Span::styled("bash(git push*)  edit(src/**)  web_fetch(domain:x)", ink(Ink::Dimmer, theme)) } else { Span::styled(text::clean(a.pattern.text()), theme.bright()) };
        put(buf, inner.x, y + 1, &Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), shown]), inner.width);
        cursor = Some((inner.x + 2 + text::width(&text::clean(&a.pattern.text()[..a.pattern.cursor()])) as u16, y + 1));
    }
    cursor
}

// ---- /hooks, /agents -------------------------------------------------------------------------------

fn hooks(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let n = state.code.hooks.as_ref().map_or(0, Vec::len);
    let h = (n as u16 * 2 + 6).min(area.height.saturating_sub(2)).max(9);
    let confirming = state.code.hooks_confirm.is_some();
    let hints: &[(&str, &str)] =
        if confirming { &[("↵", "trust them"), ("esc", "no")] } else { &[("t", "trust the project's hooks"), ("↑↓", "select"), ("esc", "close")] };
    let inner = frame(area, 100, h.max(if confirming { 14 } else { 0 }), "hooks", hints, theme, buf);
    let Some(hooks) = &state.code.hooks else {
        let msg = if unavailable(state, "list_hooks") { "this daemon does not serve hooks yet" } else { "loading…" };
        note(inner, inner.y, msg, theme, buf);
        return;
    };
    if hooks.is_empty() {
        note(inner, inner.y, "no hooks configured · add them under [hooks] in settings, or in .claude/settings.json", theme, buf);
    }
    // Trusting names everything it would trust, and says so before it does.
    let reserve = if confirming { 5usize } else { 0 };
    if confirming {
        let todo = state.untrusted_project_hooks();
        let y0 = inner.bottom().saturating_sub(reserve as u16);
        let amber = ink(Ink::Amber, theme);
        put(
            buf,
            inner.x,
            y0,
            &Line::from(Span::styled(format!("trust all {} project hook{}? they run commands as you", todo.len(), if todo.len() == 1 { "" } else { "s" }), amber)),
            inner.width,
        );
        for (i, hk) in todo.iter().take(reserve - 2).enumerate() {
            let line = format!("{} {} · {}", text::clean(&hk.event), text::clean(hk.matcher.as_deref().unwrap_or("*")), text::clean(&hk.command));
            put(buf, inner.x + 2, y0 + 1 + i as u16, &Line::from(Span::styled(text::truncate(&line, inner.width as usize - 2), theme.normal())), inner.width.saturating_sub(2));
        }
        let digest = state.code.hooks_confirm.as_deref().unwrap_or("");
        let more = if todo.len() > reserve - 2 { format!("and {} more · ", todo.len() - (reserve - 2)) } else { String::new() };
        note(inner, y0 + reserve as u16 - 1, &format!("{more}digest {}", text::truncate(digest, 16)), theme, buf);
    }
    let visible = (inner.height as usize).saturating_sub(reserve) / 2;
    let first = first_row(sel, visible);
    for (i, hk) in hooks.iter().enumerate().skip(first).take(visible) {
        let s = i == sel;
        let y = inner.y + (i - first) as u16 * 2;
        let (state_word, st) = match (hk.scope, hk.trusted) {
            (SettingsScope::User, _) => ("runs", Ink::Green),
            (_, true) => ("trusted", Ink::Green),
            (_, false) => ("not trusted — does not run", Ink::Amber),
        };
        let spans = vec![
            row_marker(s, false, theme),
            Span::styled(text::pad(&text::clean(&hk.event), 18), if s { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
            Span::styled(text::pad(&text::clean(hk.matcher.as_deref().unwrap_or("*")), 20), ink(Ink::Dim, theme)),
            Span::styled(scope_label(hk.scope), ink(scope_ink(hk.scope), theme)),
            Span::styled("  ", ink(Ink::Dim, theme)),
            Span::styled(state_word, ink(st, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
        let cmd = Line::from(Span::styled(text::truncate(&text::clean(&hk.command), inner.width as usize - 4), ink(Ink::Dimmer, theme)));
        put(buf, inner.x + 2, y + 1, &cmd, inner.width.saturating_sub(2));
    }
}

fn agents(area: Rect, state: &State, sel: usize, open: bool, theme: &Theme, buf: &mut Buffer) {
    let n = state.code.agents.as_ref().map_or(0, Vec::len);
    let h = (n as u16 + 14).min(area.height.saturating_sub(2)).max(10);
    let inner = frame(area, 100, h, "agents", &[("↵", "details"), ("↑↓", "select"), ("esc", "close")], theme, buf);
    let Some(list) = &state.code.agents else {
        let msg = if unavailable(state, "list_agent_types") { "this daemon does not list agent types yet" } else { "loading…" };
        note(inner, inner.y, msg, theme, buf);
        return;
    };
    let detail_rows = if open { 6 } else { 0 };
    let visible = (inner.height as usize).saturating_sub(detail_rows).max(1);
    let first = first_row(sel, visible);
    for (i, a) in list.iter().enumerate().skip(first).take(visible) {
        let s = i == sel;
        let y = inner.y + (i - first) as u16;
        let spans = vec![
            row_marker(s, false, theme),
            Span::styled(text::pad(&text::clean(&a.name), 22), if s { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
            Span::styled(text::pad(&text::clean(a.model.as_deref().unwrap_or("inherit")), 14), ink(Ink::Dim, theme)),
            Span::styled(text::truncate(&text::short_path(&text::clean(&a.source)), (inner.width as usize).saturating_sub(40)), ink(Ink::Dimmer, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
    if let (true, Some(a)) = (open, list.get(sel)) {
        let y0 = inner.bottom().saturating_sub(detail_rows as u16);
        let tools = if a.tools.is_empty() { "the caller's tools, without agent".to_string() } else { a.tools.join(", ") };
        let mut body = vec![text::clean(&a.description)];
        body.push(format!("tools: {tools}"));
        let mut y = y0;
        for l in body.iter().flat_map(|b| text::wrap(b, inner.width as usize)) {
            if y >= inner.bottom() {
                break;
            }
            put(buf, inner.x, y, &Line::from(Span::styled(l, ink(Ink::Dim, theme))), inner.width);
            y += 1;
        }
    }
}

// ---- /mcp ------------------------------------------------------------------------------------------

fn mcp(area: Rect, state: &State, m: &McpUi, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let n = state.code.mcp.as_ref().map_or(0, Vec::len);
    let h = (n as u16 + 9).min(area.height.saturating_sub(2)).max(10);
    let hints: &[(&str, &str)] = if m.add.is_some() {
        &[("↵", "add"), ("tab", "layer"), ("esc", "cancel")]
    } else {
        &[("a", "add"), ("d", "remove"), ("r", "refresh"), ("esc", "close")]
    };
    let inner = frame(area, 100, h, "mcp servers", hints, theme, buf);
    let Some(list) = &state.code.mcp else {
        let msg = if unavailable(state, "mcp_list") { "this daemon does not manage MCP servers yet" } else { "loading…" };
        note(inner, inner.y, msg, theme, buf);
        return None;
    };
    if list.is_empty() {
        note(inner, inner.y, "no servers · a adds one: name command args…  or  name https://host/mcp", theme, buf);
    }
    let reserve = if m.add.is_some() { 4 } else { 0 };
    let visible = (inner.height as usize).saturating_sub(reserve);
    let first = first_row(m.sel, visible);
    for (i, s) in list.iter().enumerate().skip(first).take(visible) {
        let sel = i == m.sel;
        let y = inner.y + (i - first) as u16;
        let status = text::clean(&s.status);
        let st = if status.starts_with("connected") {
            Ink::Green
        } else if status.starts_with("failed") {
            Ink::Red
        } else if status.starts_with("needs") {
            Ink::Amber
        } else {
            Ink::Dimmer
        };
        let spans = vec![
            row_marker(sel, false, theme),
            Span::styled(text::pad(&text::clean(&s.name), 20), if sel { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
            Span::styled(text::pad(&s.transport, 7), ink(Ink::Dim, theme)),
            Span::styled(text::pad(scope_label(s.scope), 9), ink(scope_ink(s.scope), theme)),
            Span::styled(text::pad(&format!("{} tools", s.tools), 10), ink(Ink::Dim, theme)),
            Span::styled(text::truncate(&status, (inner.width as usize).saturating_sub(50)), ink(st, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
    let mut cursor = None;
    if let Some(a) = &m.add {
        let y = inner.bottom().saturating_sub(3);
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![Span::styled("new server in ", ink(Ink::Dim, theme)), Span::styled(scope_label(a.scope), ink(scope_ink(a.scope), theme))]),
            inner.width,
        );
        let shown = if a.line.text().is_empty() {
            Span::styled("files npx -y @modelcontextprotocol/server-filesystem .", ink(Ink::Dimmer, theme))
        } else {
            Span::styled(text::clean(a.line.text()), theme.bright())
        };
        put(buf, inner.x, y + 1, &Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), shown]), inner.width);
        if let Some(e) = &a.error {
            put(buf, inner.x, y + 2, &Line::from(Span::styled(text::clean(e), ink(Ink::Amber, theme))), inner.width);
        }
        cursor = Some((inner.x + 2 + text::width(&text::clean(&a.line.text()[..a.line.cursor()])) as u16, y + 1));
    }
    cursor
}

// ---- /memory ---------------------------------------------------------------------------------------

fn instructions(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let n = state.code.instructions.as_ref().map_or(0, Vec::len);
    let h = (n as u16 + 6).min(area.height.saturating_sub(2)).max(8);
    let inner = frame(area, 100, h, "instructions", &[("↵", "edit"), ("r", "reload"), ("↑↓", "select"), ("esc", "close")], theme, buf);
    note(inner, inner.y, "Marlowe reads these at the start of a session; editing reloads them into this one.", theme, buf);
    let Some(files) = &state.code.instructions else {
        note(inner, inner.y + 2, "looking…", theme, buf);
        return;
    };
    let visible = (inner.height as usize).saturating_sub(2).max(1);
    let first = first_row(sel, visible);
    for (i, f) in files.iter().enumerate().skip(first).take(visible) {
        let s = i == sel;
        let y = inner.y + 2 + (i - first) as u16;
        let spans = vec![
            row_marker(s, false, theme),
            Span::styled(text::pad(&f.scope, 8), ink(Ink::Dim, theme)),
            Span::styled(
                text::truncate_left(&text::short_path(&text::clean(&f.path)), (inner.width as usize).saturating_sub(22)),
                if s { theme.bright() } else { theme.normal() },
            ),
            Span::styled(if f.exists { "" } else { "  new" }, ink(Ink::Dimmer, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
}

// ---- /context --------------------------------------------------------------------------------------

/// Tokens a string takes, roughly: four characters each.
fn est(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(4)
}

/// What the conversation holds, by kind, estimated from what is on screen.
pub fn breakdown(state: &State) -> Vec<(&'static str, u64)> {
    let (mut user, mut reply, mut think, mut tools, mut recall) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for e in &state.entries {
        match e {
            Entry::User { text, .. } => user += est(text),
            Entry::Prose { text } | Entry::Brief { text, .. } => reply += est(text),
            Entry::Thinking { tokens, text, .. } => think += if *tokens > 0 { *tokens } else { est(text) },
            Entry::Tool { call_id, summary, .. } => {
                tools += est(summary);
                if let Some(d) = state.code.tool_detail.get(call_id) {
                    tools += est(&d.output) + est(&d.started);
                }
            }
            Entry::Job { output, command, .. } => tools += est(output) + est(command),
            Entry::Recall { summary, .. } => recall += est(summary),
            _ => {}
        }
    }
    vec![("your messages", user), ("replies", reply), ("tool calls and results", tools), ("thinking", think), ("recalled memories", recall)]
}

fn context(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    // The daemon's own parts when it sends them; otherwise an estimate, said to be one.
    let daemon = state.code.context_breakdown.as_deref();
    let parts: Vec<(String, u64)> = match daemon {
        Some(p) => p.to_vec(),
        None => breakdown(state).into_iter().map(|(l, n)| (l.to_string(), n)).collect(),
    };
    let notices = &state.code.context_notices;
    let h = (parts.len() + notices.len() + 12) as u16;
    let inner = frame(area, 84, h.min(area.height.saturating_sub(2)), "context", &[("c", "compact now"), ("p", "hardware plan"), ("esc", "close")], theme, buf);
    let dim = ink(Ink::Dim, theme);
    let mut y = inner.y;
    match state.meter() {
        Some(m) => {
            put(buf, inner.x, y, &super::context::meter_line(&m, inner.width as usize, theme), inner.width);
            y += 1;
            let frac = if m.window == 0 { 0.0 } else { m.used as f64 / f64::from(m.window) };
            put(buf, inner.x, y, &Line::from(Span::styled(format!("{:.0}% of the window used", frac * 100.0), dim)), inner.width);
            y += 2;
        }
        None => {
            note(inner, y, "no model yet — add one in the library", theme, buf);
            y += 2;
        }
    }
    let heading = if daemon.is_some() { "what fills the window" } else { "what this screen holds (an estimate; the daemon has not sent its own count)" };
    put(buf, inner.x, y, &Line::from(Span::styled(heading, ink(Ink::Accent, theme))), inner.width);
    y += 1;
    let total: u64 = parts.iter().map(|(_, n)| n).sum::<u64>().max(1);
    for (label, n) in parts {
        let pct = n * 100 / total;
        let bar = "━".repeat(((n * 24).div_ceil(total)).min(24) as usize);
        let line = Line::from(vec![
            Span::styled(text::pad(&text::truncate(&text::clean(&label), 23), 24), theme.normal()),
            Span::styled(text::pad(&text::thousands(n), 8), dim),
            Span::styled(text::pad(&format!("{pct}%"), 5), ink(Ink::Dimmer, theme)),
            Span::styled(bar, ink(Ink::Accent, theme)),
        ]);
        put(buf, inner.x, y, &line, inner.width);
        y += 1;
    }
    for n in notices {
        put(buf, inner.x, y, &Line::from(Span::styled(text::truncate(&text::clean(n), inner.width as usize), ink(Ink::Amber, theme))), inner.width);
        y += 1;
    }
    y += 1;
    let todos = state.code.open_todos();
    let extras = [
        format!("{} open todo{}", todos, if todos == 1 { "" } else { "s" }),
        format!("{} attachment{}", state.code.attachments.len(), if state.code.attachments.len() == 1 { "" } else { "s" }),
        format!("{} queued", state.code.queue_len()),
    ];
    put(buf, inner.x, y, &Line::from(Span::styled(extras.join(" · "), ink(Ink::Dimmer, theme))), inner.width);
    y += 1;
    if daemon.is_none() {
        note(inner, y, "the system prompt, tool schemas and instruction files count in the total above but are not itemised here", theme, buf);
    }
}

// ---- /cost -----------------------------------------------------------------------------------------

fn cost(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let rows = state.code.cost.as_ref().map_or(0, |c| c.by_model.len());
    let inner = frame(area, 84, (rows as u16 + 10).min(area.height.saturating_sub(2)), "cost", &[("esc", "close")], theme, buf);
    let dim = ink(Ink::Dim, theme);
    let t = state.code.totals;
    let mut y = inner.y;
    match &state.code.cost {
        Some(c) => {
            put(
                buf,
                inner.x,
                y,
                &Line::from(vec![
                    Span::styled(format!("{} model calls", c.calls), theme.normal()),
                    Span::styled(" · ", ink(Ink::Dimmer, theme)),
                    Span::styled(format!("{} in", text::compact(c.usage.input)), dim),
                    Span::styled(format!(" · {} out", text::compact(c.usage.output)), dim),
                    Span::styled(format!(" · {} cached", text::compact(c.usage.cached_input)), dim),
                    Span::styled(c.usage.cost_usd.map_or(String::new(), |u| format!(" · {}", text::cost(u))), theme.bright()),
                ]),
                inner.width,
            );
            y += 2;
            for m in &c.by_model {
                let line = Line::from(vec![
                    Span::styled(text::pad(&text::clean(&m.model.model_id), 36), theme.normal()),
                    Span::styled(text::pad(&format!("{} calls", m.calls), 10), dim),
                    Span::styled(text::pad(&format!("{} in", text::compact(m.usage.input)), 10), dim),
                    Span::styled(text::pad(&format!("{} out", text::compact(m.usage.output)), 10), dim),
                    Span::styled(m.usage.cost_usd.map_or("local".to_string(), text::cost), ink(Ink::Dimmer, theme)),
                ]);
                put(buf, inner.x, y, &line, inner.width);
                y += 1;
            }
        }
        None => {
            let msg = if unavailable(state, "session_cost") { "the daemon does not total this session yet; this is what the screen saw" } else { "asking the daemon…" };
            note(inner, y, msg, theme, buf);
            y += 2;
            let line = Line::from(vec![
                Span::styled(format!("{} turns", t.turns), theme.normal()),
                Span::styled(format!(" · {} in · {} out", text::compact(t.input), text::compact(t.output)), dim),
                Span::styled(format!(" · {}", text::cost(t.usd)), theme.bright()),
            ]);
            put(buf, inner.x, y, &line, inner.width);
        }
    }
}

// ---- /rewind ---------------------------------------------------------------------------------------

fn rewind(area: Rect, state: &State, r: &RewindUi, theme: &Theme, buf: &mut Buffer) {
    let n = state.code.checkpoints.as_ref().map_or(0, Vec::len);
    let h = (n as u16 + 8).min(area.height.saturating_sub(2)).max(10);
    let hints: &[(&str, &str)] = if r.scope.is_some() { &[("↵", "rewind"), ("↑↓", "what"), ("esc", "back")] } else { &[("↵", "choose"), ("↑↓", "select"), ("esc", "close")] };
    let inner = frame(area, 90, h, "rewind", hints, theme, buf);
    let Some(list) = &state.code.checkpoints else {
        let msg = if unavailable(state, "list_checkpoints") { "this daemon keeps no checkpoints yet" } else { "loading…" };
        note(inner, inner.y, msg, theme, buf);
        return;
    };
    if list.is_empty() {
        note(inner, inner.y, "no checkpoints yet — one is kept for each turn that changes files", theme, buf);
        return;
    }
    if let Some((turn, sel)) = r.scope {
        put(buf, inner.x, inner.y, &Line::from(Span::styled(format!("go back to turn {turn}"), theme.bright())), inner.width);
        for (i, (_, label, help)) in REWIND_SCOPES.iter().enumerate() {
            let s = i == sel;
            let line = Line::from(vec![
                row_marker(s, false, theme),
                Span::styled(text::pad(label, 24), if s { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
                Span::styled(*help, ink(Ink::Dim, theme)),
            ]);
            put(buf, inner.x, inner.y + 2 + i as u16, &line, inner.width);
        }
        note(inner, inner.y + 6, "the files Marlowe's tools changed are restored; your repository's history is not touched", theme, buf);
        return;
    }
    let visible = inner.height as usize;
    let first = first_row(r.sel, visible);
    for (i, c) in list.iter().enumerate().skip(first).take(visible) {
        let s = i == r.sel;
        let spans = vec![
            row_marker(s, false, theme),
            Span::styled(text::pad(&format!("turn {}", c.turn), 9), if s { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
            Span::styled(text::pad(&format!("{} file{}", c.files, if c.files == 1 { "" } else { "s" }), 10), ink(Ink::Dim, theme)),
            Span::styled(text::pad(&text::ago(c.at_ms, state.now_ms), 11), ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&text::clean(&c.summary), (inner.width as usize).saturating_sub(34)), ink(Ink::Dim, theme)),
        ];
        put_row(state, buf, (inner.x, inner.y + (i - first) as u16, inner.width), Line::from(spans), Target::Row(i), theme);
    }
}

// ---- /diff -----------------------------------------------------------------------------------------

/// The viewer's lines and the row each file starts on.
pub fn diff_lines(d: &DiffUi, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    match &d.diff {
        Some(diff) => {
            for f in &diff.files {
                starts.push(lines.len());
                let one = crate::diff::Diff { files: vec![f.clone()] };
                lines.extend(crate::diff::render(&one, crate::diff::Layout::full(width), theme).lines);
                lines.push(Line::default());
            }
        }
        None => {
            for l in d.raw.lines() {
                lines.push(Line::from(Span::styled(text::truncate(&text::clean(l), width), ink(Ink::Dim, theme))));
            }
        }
    }
    (lines, starts)
}

fn diff(area: Rect, d: &DiffUi, theme: &Theme, buf: &mut Buffer) {
    let w = area.width.saturating_sub(6).clamp(60, 150);
    let h = area.height.saturating_sub(2);
    let r = centred(area, w, h, false);
    let inner = panel(r, &format!("diff · {}", d.title), ink(Ink::Accent, theme), false, buf);
    panel_hints(r, &[("↑↓", "scroll"), ("n p", "next/previous file"), ("esc", "close")], theme, buf);
    if d.loading {
        note(inner, inner.y, "asking the daemon…", theme, buf);
        return;
    }
    let (lines, starts) = diff_lines(d, inner.width as usize, theme);
    if lines.is_empty() {
        note(inner, inner.y, "nothing changed", theme, buf);
        return;
    }
    let rows = inner.height as usize;
    let max = lines.len().saturating_sub(rows);
    d.scroll.max.set(max);
    *d.starts.borrow_mut() = starts;
    let top = d.scroll.at();
    for (i, l) in lines.iter().skip(top).take(rows).enumerate() {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
    if lines.len() > rows {
        let tag = format!(" {}–{} of {} ", top + 1, (top + rows).min(lines.len()), lines.len());
        let tw = text::width(&tag) as u16;
        put(buf, r.right().saturating_sub(tw + 2), r.y, &Line::from(Span::styled(tag, ink(Ink::Dimmer, theme))), tw);
    }
}

// ---- /jobs -----------------------------------------------------------------------------------------

fn jobs(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let n = state.code.jobs.len();
    let inner = frame(area, 100, (n as u16 + 6).min(area.height.saturating_sub(2)).max(8), "background jobs", &[("x", "stop"), ("↑↓", "select"), ("esc", "close")], theme, buf);
    if n == 0 {
        let msg = if unavailable(state, "list_jobs") { "this daemon does not run background jobs yet" } else { "none — ctrl+b moves a running command here" };
        note(inner, inner.y, msg, theme, buf);
        return;
    }
    let visible = inner.height as usize;
    let first = first_row(sel, visible);
    for (i, j) in state.code.jobs.iter().enumerate().skip(first).take(visible) {
        let s = i == sel;
        let (word, ik) = match j.exit_code {
            None => ("running".to_string(), Ink::Green),
            Some(0) => ("done".to_string(), Ink::Dim),
            Some(c) => (format!("exit {c}"), Ink::Red),
        };
        let spans = vec![
            row_marker(s, false, theme),
            Span::styled(text::pad(&text::clean(&j.id), 10), ink(Ink::Dimmer, theme)),
            Span::styled(text::pad(&word, 10), ink(ik, theme)),
            Span::styled(text::pad(&text::ago(j.started_ms, state.now_ms), 11), ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&text::clean(&j.command), (inner.width as usize).saturating_sub(34)), if s { theme.bright() } else { theme.normal() }),
        ];
        put_row(state, buf, (inner.x, inner.y + (i - first) as u16, inner.width), Line::from(spans), Target::Row(i), theme);
    }
}

// ---- ask_user --------------------------------------------------------------------------------------

fn question(area: Rect, q: &QuestionUi, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let cur = &q.questions[q.q];
    let detail = |i: usize| cur.option_details.get(i);
    let preview_of = |i: usize| detail(i).and_then(|d| d.preview.as_deref()).filter(|p| !p.trim().is_empty());
    let has_previews = (0..cur.options.len()).any(|i| preview_of(i).is_some());
    // A preview sits beside the options when the window is wide enough, else under them.
    let wide = has_previews && area.width >= 130;
    let w: u16 = if wide { area.width.saturating_sub(8).min(124) } else { 84 };
    let tw = (w - 4) as usize;
    let left_w = if wide { 48usize } else { tw };
    let q_lines = text::wrap(&text::clean(&cur.question), tw);
    // Each option is a row, and its description (two lines at most) under it.
    let desc_lines = |i: usize| -> Vec<String> {
        detail(i)
            .map(|d| text::wrap(&text::clean(&d.description), left_w.saturating_sub(7)).into_iter().take(2).collect())
            .unwrap_or_default()
    };
    let option_rows: usize = (0..cur.options.len()).map(|i| 1 + desc_lines(i).len()).sum();
    // The tallest preview any option has, so the box does not jump as the focus moves.
    let tallest = (0..cur.options.len()).filter_map(preview_of).map(|p| p.lines().count().min(PREVIEW_ROWS) + 2).max().unwrap_or(0);
    let body_rows = if wide { option_rows.max(tallest) } else { option_rows + tallest };
    let h = (q_lines.len() + body_rows + 9) as u16;
    let r = centred(area, w, h, false);
    let amber = ink(Ink::Amber, theme);
    let inner = panel(r, "question", amber, true, buf);
    let hint: &[(&str, &str)] = if q.typing {
        &[("\u{21b5}", "done"), ("esc", "back")]
    } else if cur.multi_select {
        &[("space", "toggle"), ("\u{21b5}", "next"), ("esc", "skip")]
    } else {
        &[("\u{21b5}", "choose"), ("1-9", "pick"), ("esc", "skip")]
    };
    panel_hints(r, hint, theme, buf);
    let mut y = inner.y;
    if q.questions.len() > 1 {
        let mut tabs = Vec::new();
        for (i, qq) in q.questions.iter().enumerate() {
            let label = if qq.header.trim().is_empty() { format!("Q{}", i + 1) } else { text::truncate(&text::clean(&qq.header), 14) };
            let mark = if q.answered(i) { "\u{2713} " } else { "" };
            let st = if i == q.q { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { ink(Ink::Dim, theme) };
            tabs.push(Span::styled(format!("{mark}{label}"), st));
            tabs.push(Span::raw("   "));
        }
        put(buf, inner.x, y, &Line::from(tabs), inner.width);
        y += 2;
    }
    for l in &q_lines {
        put(buf, inner.x, y, &Line::from(Span::styled(l.clone(), theme.bright())), inner.width);
        y += 1;
    }
    y += 1;
    let opts_top = y;
    let col = left_w as u16;
    for (i, o) in cur.options.iter().enumerate() {
        let on = i == q.cursor[q.q];
        let picked = q.picked[q.q][i];
        let box_ = if cur.multi_select { if picked { "[x] " } else { "[ ] " } } else if picked { "(\u{2022}) " } else { "( ) " };
        let line = Line::from(vec![
            row_marker(on, false, theme),
            Span::styled(format!("{} ", i + 1), ink(Ink::Dimmer, theme)),
            Span::styled(box_, ink(if picked { Ink::Accent } else { Ink::Dim }, theme)),
            Span::styled(text::truncate(&text::clean(o), left_w.saturating_sub(10)), if on { theme.bright() } else { theme.normal() }),
        ]);
        put(buf, inner.x, y, &line, col.min(inner.width));
        y += 1;
        for d in desc_lines(i) {
            put(buf, inner.x + 7, y, &Line::from(Span::styled(d, ink(Ink::Dim, theme))), col.saturating_sub(7).min(inner.width));
            y += 1;
        }
    }
    let other_on = q.cursor[q.q] == cur.options.len();
    let other = &q.other[q.q];
    let shown = if other.text().is_empty() && !q.typing { Span::styled("something else\u{2026}", ink(Ink::Dimmer, theme)) } else { Span::styled(text::clean(other.text()), theme.bright()) };
    put(buf, inner.x, y, &Line::from(vec![row_marker(other_on, false, theme), Span::styled("   ", ink(Ink::Dim, theme)), shown]), col.min(inner.width));
    let cursor = q.typing.then(|| (inner.x + 5 + text::width(&text::clean(&other.text()[..other.cursor()])) as u16, y));
    y += 2;
    // The focused option's preview: a monospace block, in the terminal's own font.
    let focus = q.cursor[q.q];
    if let Some(p) = preview_of(focus) {
        let (px, py, pw, rows) = if wide {
            let px = inner.x + col + 2;
            (px, opts_top, inner.right().saturating_sub(px), PREVIEW_ROWS.max(option_rows + 2))
        } else {
            (inner.x, y, inner.width, PREVIEW_ROWS)
        };
        put(buf, px, py, &Line::from(Span::styled("preview", ink(Ink::Dimmer, theme))), pw);
        let all: Vec<&str> = p.lines().collect();
        for (i, l) in all.iter().take(rows).enumerate() {
            let rule = format!("{} ", chrome::QUOTE_RULE);
            let line = Line::from(vec![Span::styled(rule, ink(Ink::Dimmer, theme)), Span::styled(text::truncate(&text::clean(l), pw.saturating_sub(2) as usize), theme.normal())]);
            put(buf, px, py + 1 + i as u16, &line, pw);
        }
        if all.len() > rows {
            put(buf, px, py + 1 + rows as u16, &Line::from(Span::styled(format!("\u{2026} {} more lines", all.len() - rows), ink(Ink::Dimmer, theme))), pw);
        }
        if !wide {
            y += rows.min(all.len()) as u16 + 2;
        }
    }
    if q.questions.len() > 1 {
        let left = (0..q.questions.len()).filter(|i| !q.answered(*i)).count();
        let msg = if left == 0 { "all answered \u{2014} \u{21b5} sends".to_string() } else { format!("{left} left \u{b7} \u{2190}\u{2192} between questions") };
        put(buf, inner.x, y.min(inner.bottom().saturating_sub(1)), &Line::from(Span::styled(msg, ink(Ink::Dimmer, theme))), inner.width);
    }
    cursor
}

/// Rows of a preview shown at once.
const PREVIEW_ROWS: usize = 10;
