//! The Library view: onboarding, slots, models, provider list, key entry, catalog search.

use marlowe_model::{ModelInfo, ProviderKind};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{hovered, ink, put, put_row};
use crate::library::{LibFocus, LibStep, Library, OverviewRow};
use crate::state::{State, Target};
use crate::text;

/// The content area inside the main frame.
fn content(main: Rect) -> Rect {
    Rect {
        x: main.x + 3,
        y: main.y + 2,
        width: main.width.saturating_sub(6),
        height: main.height.saturating_sub(4),
    }
}

pub fn draw(main: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let lib = &state.library;
    let c = content(main);
    // The hints name what ↵ does on the row under the cursor, and how to leave.
    let hints: Vec<(&str, String)> = match &lib.step {
        LibStep::Overview if !lib.has_models() => {
            empty(c, state, theme, buf);
            vec![("↵", "add a model".into()), ("esc/⌫", "back".into())]
        }
        LibStep::Overview => {
            overview(c, state, theme, buf);
            let enter = lib.enter_action();
            match lib.overview_row() {
                OverviewRow::Slot(_) => vec![
                    ("↵", enter),
                    ("←→", "model".into()),
                    ("↑↓", "move".into()),
                    ("a", "add a model".into()),
                    ("esc/⌫", "back".into()),
                ],
                OverviewRow::Model(_) => vec![
                    ("↵", enter),
                    ("c", "context".into()),
                    ("d", "remove".into()),
                    ("↑↓", "move".into()),
                    ("a", "add a model".into()),
                    ("esc/⌫", "back".into()),
                ],
                OverviewRow::Add => vec![("↵", enter), ("↑↓", "move".into()), ("esc/⌫", "back".into())],
            }
        }
        LibStep::Providers { sel } => {
            providers(c, state, *sel, theme, buf);
            vec![("↑↓", "select".into()), ("↵", "choose".into()), ("esc/⌫", "back".into())]
        }
        LibStep::Key { provider, input } => {
            key_entry(c, *provider, input.len(), theme, buf);
            vec![("↵", "save and continue".into()), ("^u", "clear".into()), ("esc", "back".into())]
        }
        LibStep::Catalog { provider, query, sel } => {
            catalog(c, state, *provider, query, *sel, theme, buf);
            vec![("type", "to search".into()), ("↑↓", "select".into()), ("↵", "add to library".into()), ("esc", "back".into())]
        }
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, v)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", ink(Ink::Dimmer, theme)));
        }
        spans.push(Span::styled(*k, ink(Ink::Dim, theme)));
        spans.push(Span::styled(format!(" {v}"), ink(Ink::Dimmer, theme)));
    }
    spans.push(Span::raw(" "));
    let l = Line::from(spans);
    let w = l.width() as u16;
    put(buf, main.right().saturating_sub(w + 2), main.bottom() - 1, &l, w);
    if let Some(f) = &state.flash {
        let max = main.width.saturating_sub(w + 8);
        let fl = Line::from(Span::styled(
            format!(" {} ", text::truncate(&text::clean(&f.text), max as usize)),
            super::note_style(f.tone, theme),
        ));
        put(buf, main.x + 2, main.bottom() - 1, &fl, max + 2);
    }
}

/// Where the terminal cursor sits in the library, if anywhere.
pub fn cursor(main: Rect, state: &State) -> Option<(u16, u16)> {
    let c = content(main);
    match &state.library.step {
        LibStep::Key { input, .. } => Some((c.x + 7 + input.len().min(48) as u16, c.y + 6)),
        LibStep::Catalog { query, .. } => Some((c.x + 2 + text::width(query) as u16, c.y + 2)),
        _ => None,
    }
}

fn heading(title: &str, sub: &str, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(title.to_string(), ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))];
    if !sub.is_empty() {
        spans.push(Span::styled(format!("  ·  {sub}"), ink(Ink::Dim, theme)));
    }
    Line::from(spans)
}

/// Draws `lines` centred in `r`; returns the first row.
fn centred(r: Rect, lines: &[Line], buf: &mut Buffer) -> u16 {
    let top = r.y + r.height.saturating_sub(lines.len() as u16) * 2 / 5;
    for (i, l) in lines.iter().enumerate() {
        let w = l.width() as u16;
        put(buf, r.x + r.width.saturating_sub(w) / 2, top + i as u16, l, r.width);
    }
    top
}

fn empty(c: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let dim = ink(Ink::Dim, theme);
    let detected: Vec<&str> = state
        .library
        .providers
        .iter()
        .filter(|p| p.detected && p.available_now)
        .map(|p| p.kind.display_name())
        .collect();
    let mut lines = vec![
        Line::from(Span::styled("You have no models. Add one?", theme.bright())),
        Line::from(""),
        Line::from(Span::styled("Every slot — the conductor, the crew, reflection — runs on a model you choose.", dim)),
        Line::from(Span::styled("The first one you add becomes the default for all of them.", dim)),
        Line::from(""),
    ];
    if !detected.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("● ", ink(Ink::Green, theme)),
            Span::styled(format!("found {}", detected.join(" and ")), dim),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(""));
    let button = lines.len();
    lines.push(Line::from(vec![
        Span::styled("↵", ink(Ink::Accent, theme)),
        Span::styled(" add a model", dim),
    ]));
    let top = centred(c, &lines, buf);
    let w = lines[button].width() as u16;
    let x = c.x + c.width.saturating_sub(w) / 2;
    put_row(state, buf, (x, top + button as u16, w), lines[button].clone(), Target::AddModel, theme);
}

fn row_marker(selected: bool, focused: bool, theme: &Theme) -> Span<'static> {
    match (selected, focused) {
        (true, true) => Span::styled("› ", ink(Ink::Accent, theme)),
        (true, false) => Span::styled("› ", ink(Ink::Dimmer, theme)),
        _ => Span::raw("  "),
    }
}

/// The marker for a row that may be under the pointer: the cursor's wins, then the hover tone.
fn marker(selected: bool, focused: bool, over: bool, theme: &Theme) -> Span<'static> {
    if over && !(selected && focused) {
        return Span::styled("› ", ink(Ink::Hover, theme));
    }
    row_marker(selected, focused, theme)
}

fn overview(c: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let lib = &state.library;
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let mut y = c.y;
    let slots_focus = lib.focus == LibFocus::Slots;
    put(buf, c.x, y, &heading("SLOTS", "which model does what", theme), c.width);
    y += 2;
    put(
        buf,
        c.x + 2,
        y,
        &Line::from(Span::styled(
            format!("{}{}{}{}", text::pad("slot", 20), text::pad("model", 34), text::pad("provider", 14), "effort"),
            dimmer,
        )),
        c.width,
    );
    y += 1;
    for (i, row) in lib.slot_rows().iter().enumerate() {
        let selected = i == lib.slot_sel;
        let label_style = if selected && slots_focus { theme.bright() } else { theme.normal() };
        let (model, provider) = match &row.model {
            Some(m) => (text::short_model(m, lib.info(m)), m.provider.display_name().to_string()),
            None => ("—".to_string(), String::new()),
        };
        let levels = row.model.as_ref().and_then(|m| lib.info(m)).map(|i| i.effort_levels.len()).unwrap_or(0);
        let effort = if levels == 0 { "—".to_string() } else { row.effort.to_string() };
        let over = hovered(state, &Target::Slot(i));
        let mut spans = vec![
            marker(selected, slots_focus, over, theme),
            Span::styled(text::pad(&row.slot.label(), 20), label_style),
            Span::styled(text::pad(&model, 34), theme.normal()),
            Span::styled(text::pad(&provider, 14), dim),
            Span::styled(text::pad(&effort, 9), ink(Ink::Speech, theme)),
        ];
        if row.inherited {
            spans.push(Span::styled("default", dimmer));
        }
        put_row(state, buf, (c.x, y, c.width), Line::from(spans), Target::Slot(i), theme);
        y += 1;
    }

    y += 2;
    let models_focus = lib.focus == LibFocus::Models;
    let n = lib.models.len();
    let sub = if n == 1 { "1 in the library · the default for every slot".to_string() } else { format!("{n} in the library") };
    put(buf, c.x, y, &heading("MODELS", &sub, theme), c.width);
    y += 2;
    put(
        buf,
        c.x + 2,
        y,
        &Line::from(Span::styled(
            format!(
                "{}{}{}{}{}",
                text::pad("model", 34),
                text::pad("provider", 14),
                text::pad("window", 16),
                text::pad("price / Mtok", 18),
                "can"
            ),
            dimmer,
        )),
        c.width,
    );
    y += 1;
    // One row stays for "add another model".
    let room = c.bottom().saturating_sub(y + 1) as usize;
    let first = lib.model_sel.min(lib.models.len().saturating_sub(1)).saturating_sub(room.saturating_sub(1));
    for (i, m) in lib.models.iter().enumerate().skip(first).take(room) {
        let selected = i == lib.model_sel;
        let name_style = if selected && models_focus { theme.bright() } else { theme.normal() };
        let over = hovered(state, &Target::Model(i));
        // The window column: the saved policy (`46k · q8 · 1×`), else the window requests use.
        let window = match state.context.model(&m.model) {
            Some(mc) => match &mc.policy {
                Some(p) => marlowe_proto::policy_label(p),
                None => marlowe_proto::ctx_label(mc.max_ctx),
            },
            None => context(&m.info),
        };
        let ctx_target = Target::ModelCtx(i);
        let wstyle = if hovered(state, &ctx_target) { ink(Ink::Hover, theme) } else { dim };
        let spans = vec![
            marker(selected, models_focus, over, theme),
            Span::styled(text::pad(&text::short_model(&m.model, Some(&m.info)), 34), name_style),
            Span::styled(text::pad(m.model.provider.display_name(), 14), dim),
            Span::styled(text::pad(&window, 16), wstyle),
            Span::styled(text::pad(&pricing(&m.info), 18), dim),
            Span::styled(abilities(&m.info), dimmer),
        ];
        put_row(state, buf, (c.x, y, c.width), Line::from(spans), Target::Model(i), theme);
        // The window cell opens the planner; recorded after the row so it wins the pointer.
        super::hit(state, Rect { x: c.x + 2 + 34 + 14, y, width: 16, height: 1 }, ctx_target);
        y += 1;
    }
    if y < c.bottom() {
        let selected = lib.overview_row() == OverviewRow::Add;
        let over = hovered(state, &Target::AddModel);
        let label = if lib.models.is_empty() { "+ add a model" } else { "+ add another model" };
        let style = if selected { theme.bright() } else { ink(Ink::Dim, theme) };
        let line = Line::from(vec![marker(selected, models_focus, over, theme), Span::styled(label, style)]);
        put_row(state, buf, (c.x, y, c.width), line, Target::AddModel, theme);
    }
}

fn context(i: &ModelInfo) -> String {
    i.context_window.map(|w| text::compact(u64::from(w))).unwrap_or_else(|| "—".into())
}

fn pricing(i: &ModelInfo) -> String {
    match (&i.pricing, i.local) {
        (_, true) => "local".into(),
        (Some(p), _) => format!("{} / {}", text::price(p.input_per_mtok), text::price(p.output_per_mtok)),
        (None, _) => "—".into(),
    }
}

fn abilities(i: &ModelInfo) -> String {
    let mut v = Vec::new();
    if i.supports_tools {
        v.push("tools");
    }
    if i.supports_reasoning {
        v.push("reasoning");
    }
    v.join(" · ")
}

fn providers(c: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let lib = &state.library;
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    put(buf, c.x, c.y, &heading("ADD A MODEL", "choose a provider", theme), c.width);
    let mut y = c.y + 3;
    for (i, kind) in ProviderKind::ALL.iter().enumerate() {
        let status = lib.provider_status(*kind);
        let avail = kind.is_available_now();
        let selected = i == sel;
        let name_style = match (avail, selected) {
            (false, _) => dimmer,
            (true, true) => theme.bright(),
            (true, false) => theme.normal(),
        };
        let over = avail && hovered(state, &Target::Row(i));
        let mut spans = vec![marker(selected && avail, true, over, theme), Span::styled(text::pad(kind.display_name(), 16), name_style)];
        if !avail {
            spans.push(Span::styled("coming soon", dimmer));
        } else {
            let detected = status.is_some_and(|s| s.detected);
            let configured = status.is_some_and(|s| s.configured);
            let (dot, dot_style) = if detected || configured {
                ("● ", ink(Ink::Green, theme))
            } else {
                ("○ ", dimmer)
            };
            let note = status.and_then(|s| s.note.clone()).unwrap_or_else(|| default_note(*kind, detected, configured));
            spans.push(Span::styled(dot, dot_style));
            spans.push(Span::styled(text::pad(&text::clean(&note), 46), dim));
            spans.push(Span::styled(if kind.is_local() { "runs on this machine" } else { "hosted" }, dimmer));
        }
        if avail {
            put_row(state, buf, (c.x, y, c.width), Line::from(spans), Target::Row(i), theme);
        } else {
            put(buf, c.x, y, &Line::from(spans), c.width);
        }
        y += 2;
    }
}

fn default_note(kind: ProviderKind, detected: bool, configured: bool) -> String {
    match kind {
        ProviderKind::Ollama if detected => "running at 127.0.0.1:11434".into(),
        ProviderKind::Ollama => "not running — start Ollama, then choose it".into(),
        _ if configured => "key stored".into(),
        _ if detected => "key found in the environment".into(),
        _ => "needs an API key".into(),
    }
}

fn key_entry(c: Rect, provider: ProviderKind, len: usize, theme: &Theme, buf: &mut Buffer) {
    let dim = ink(Ink::Dim, theme);
    put(buf, c.x, c.y, &heading(&provider.display_name().to_uppercase(), "API key", theme), c.width);
    let lines = [
        format!("{} needs an API key. It is stored in Windows Credential Manager —", provider.display_name()),
        "never written to disk, never logged, and scrubbed from every tool's environment.".to_string(),
    ];
    for (i, l) in lines.iter().enumerate() {
        put(buf, c.x, c.y + 2 + i as u16, &Line::from(Span::styled(l.clone(), dim)), c.width);
    }
    let masked: String = "•".repeat(len.min(48));
    let mut spans = vec![Span::styled("key  ", ink(Ink::Accent, theme)), Span::styled(masked, theme.normal())];
    if len == 0 {
        spans.push(Span::styled("paste or type the key", ink(Ink::Dimmer, theme)));
    } else {
        spans.push(Span::styled(format!("  {len} characters"), ink(Ink::Dimmer, theme)));
    }
    put(buf, c.x + 2, c.y + 6, &Line::from(spans), c.width);
}

fn catalog(c: Rect, state: &State, provider: ProviderKind, query: &str, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let lib: &Library = &state.library;
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let rows = lib.filtered_catalog(query);
    let sub = if lib.catalog.loading {
        "loading the catalog…".to_string()
    } else {
        format!("{} of {} models", rows.len(), lib.catalog.models.len())
    };
    put(buf, c.x, c.y, &heading(&provider.display_name().to_uppercase(), &sub, theme), c.width);
    let q = if query.is_empty() {
        Line::from(vec![Span::styled("⌕ ", ink(Ink::Accent, theme)), Span::styled("search by name", dimmer)])
    } else {
        Line::from(vec![Span::styled("⌕ ", ink(Ink::Accent, theme)), Span::styled(text::clean(query), theme.normal())])
    };
    put(buf, c.x, c.y + 2, &q, c.width);

    if let Some(err) = &lib.catalog.error {
        put(buf, c.x + 2, c.y + 4, &Line::from(Span::styled(text::truncate(err, c.width as usize - 2), ink(Ink::Red, theme))), c.width);
        return;
    }
    if lib.catalog.loading {
        return;
    }
    if rows.is_empty() {
        put(buf, c.x + 2, c.y + 4, &Line::from(Span::styled("no models match", dim)), c.width);
        return;
    }
    let y0 = c.y + 4;
    put(
        buf,
        c.x + 2,
        y0,
        &Line::from(Span::styled(
            format!("{}{}{}{}", text::pad("model", 46), text::pad("context", 10), text::pad("price / Mtok", 18), "can"),
            dimmer,
        )),
        c.width,
    );
    let room = c.bottom().saturating_sub(y0 + 1) as usize;
    let first = sel.saturating_sub(room.saturating_sub(1));
    for (i, m) in rows.iter().enumerate().skip(first).take(room) {
        let selected = i == sel;
        let added = lib.in_library(provider, &m.id);
        let name_style: Style = if selected { theme.bright() } else { theme.normal() };
        let over = hovered(state, &Target::Row(i));
        let mut spans = vec![
            marker(selected, true, over, theme),
            Span::styled(text::pad(&text::clean(&m.id), 46), name_style),
            Span::styled(text::pad(&context(m), 10), dim),
            Span::styled(text::pad(&pricing(m), 18), dim),
            Span::styled(text::pad(&abilities(m), 18), dimmer),
        ];
        if added {
            spans.push(Span::styled("in library", ink(Ink::Green, theme)));
        }
        put_row(state, buf, (c.x, y0 + 1 + (i - first) as u16, c.width), Line::from(spans), Target::Row(i), theme);
    }
}
