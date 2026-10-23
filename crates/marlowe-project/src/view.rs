//! The view-model: one renderer over any project's JSON.
//!
//! [`view`] turns an item into a [`Row`] by reading its form (embedded in the state) and resolving
//! each field's display hint into a typed [`Cell`]. It knows five things and nothing about any
//! project kind: the item tree (`parent`, `order`), each item's form, the hint vocabulary, the
//! `live` and `agents` slots every item has, and the kind template's rollups ([`overview`]).
//! A kind with no registration still renders: its data is drawn field by field with no hints.
//!
//! The TUI is a thin painter of [`Row`]; [`paint`] is the plain-text painter used by tests and
//! the CLI. Colours are tone names ([`Tone`]); the painter owns the palette. All text that came
//! from outside the harness passes through the display sanitiser before it reaches a cell.

use std::collections::BTreeMap;

use marlowe_contract::{sanitize_line, sanitize_prose};
use serde_json::{Map, Value};

use crate::form::{fill_template, resolved_columns, Field, FieldSource, FieldType, Form, ShowAs, Tone};
use crate::plan::Plan;
use crate::registry::{KindTemplate, Registry, Rollup};
use crate::state::*;

/// Width of a painted bar, in cells.
pub const BAR_WIDTH: usize = 8;

/// A column header of a table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The field key.
    pub key: String,
    /// The header words.
    pub label: String,
}

/// An agent chip: activating it opens that agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChip {
    /// `a_<n>`.
    pub id: String,
    /// The role (empty when the agent is not in the state).
    pub role: String,
    /// Where it is, when known.
    pub status: Option<AgentStatus>,
}

/// One drawn value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Plain text, wrapped and clipped to `lines` lines.
    Text {
        /// The words (may hold newlines).
        text: String,
        /// Most lines to show.
        lines: u32,
    },
    /// A glyph and colour.
    Badge {
        /// The value.
        text: String,
        /// The glyph, if the value carries one.
        glyph: Option<String>,
        /// The tone.
        tone: Tone,
    },
    /// A link the host may open in the browser.
    Link {
        /// The words.
        label: String,
        /// The raw url (one key away from the screen).
        url: String,
        /// The host name, shown first.
        host: String,
        /// Only `http` and `https` urls without whitespace or control characters can be opened.
        openable: bool,
    },
    /// Rows under named columns.
    Table {
        /// The columns, in order.
        columns: Vec<Column>,
        /// One cell per column per row.
        rows: Vec<Vec<Cell>>,
    },
    /// `n/m` and a bar.
    Bar {
        /// 0 to 1.
        fraction: f64,
        /// `12/40` or `43%`.
        text: String,
    },
    /// A value against a range.
    Meter {
        /// 0 to 1 against the range, `None` for a value with no upper end (a size).
        fraction: Option<f64>,
        /// The value in words (`41%`, `1.2 GB`, `0.72`).
        text: String,
        /// Green, amber or red.
        tone: Tone,
    },
    /// `00:42:10`.
    Duration {
        /// The span.
        ms: u64,
        /// `HH:MM:SS`.
        text: String,
        /// The item is running, so the span grows.
        live: bool,
    },
    /// A moment, relative to now.
    Time {
        /// The moment.
        ms: u64,
        /// `3 min ago`.
        text: String,
    },
    /// Collapsed raw JSON.
    Json(String),
}

/// A cell with the field it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct LabeledCell {
    /// The field key.
    pub key: String,
    /// The label words.
    pub label: String,
    /// The drawn value.
    pub cell: Cell,
}

/// One item, ready to paint.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The item id (what a click acts on).
    pub id: String,
    /// The item kind.
    pub kind: String,
    /// The title line: the form's `item_label` filled in.
    pub title: String,
    /// The item's status.
    pub status: ItemStatus,
    /// The status as a badge.
    pub status_badge: Cell,
    /// The title already shows the status (the form's label has `{status}`).
    pub status_in_title: bool,
    /// The fields, in form order, hidden and empty ones left out.
    pub cells: Vec<LabeledCell>,
    /// The agents assigned to the item, on every node whatever the form says.
    pub agents: Vec<AgentChip>,
    /// The item's children.
    pub children: Vec<Row>,
}

/// The tone an item status draws in.
pub fn status_tone(s: ItemStatus) -> Tone {
    match s {
        ItemStatus::Queued | ItemStatus::Skipped | ItemStatus::Cancelled => Tone::Dim,
        ItemStatus::Running => Tone::Info,
        ItemStatus::Done => Tone::Ok,
        ItemStatus::Failed => Tone::Bad,
        ItemStatus::Blocked => Tone::Warn,
    }
}

fn status_badge(s: ItemStatus) -> Cell {
    Cell::Badge { text: s.as_str().to_string(), glyph: None, tone: status_tone(s) }
}

struct Ctx<'a> {
    plan: &'a Plan,
    now_ms: u64,
    running: bool,
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => a.iter().map(scalar_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", "),
        Value::Object(_) => v.to_string(),
    }
}

fn path_in<'a>(data: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut cur = data.get(parts.next()?)?;
    for p in parts {
        cur = cur.as_object()?.get(p)?;
    }
    Some(cur)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(3)).collect();
        t.push_str("...");
        t
    }
}

/// `HH:MM:SS` for a span in milliseconds.
pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// A relative time: `just now`, `42 s ago`, `3 min ago`, `2 h ago`, `5 d ago` (and `in ...` for
/// the future).
pub fn format_relative(ts_ms: u64, now_ms: u64) -> String {
    let (d, future) = if ts_ms > now_ms { (ts_ms - now_ms, true) } else { (now_ms - ts_ms, false) };
    let s = d / 1000;
    let words = if s < 10 {
        return "just now".to_string();
    } else if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        format!("{} h", s / 3600)
    } else {
        format!("{} d", s / 86_400)
    };
    if future {
        format!("in {words}")
    } else {
        format!("{words} ago")
    }
}

/// A byte count in words: `812 B`, `1.2 GB`.
pub fn format_bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if b < 1024.0 {
        return format!("{} B", b.round() as u64);
    }
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

fn number_text(v: f64, unit: Option<&str>) -> String {
    match unit {
        Some("bytes") => format_bytes(v),
        Some("%") => format!("{v:.0}%"),
        other => {
            let n = if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v:.2}") };
            match other {
                Some(u) => format!("{n} {u}"),
                None => n,
            }
        }
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let auth = auth.rsplit('@').next().unwrap_or(auth);
    auth.split(':').next().unwrap_or(auth).to_string()
}

fn openable(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && !url.chars().any(|c| c.is_whitespace() || c.is_control()) && !host_of(url).is_empty()
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => scalar_text(a).cmp(&scalar_text(b)),
    }
}

fn cell_for(field: &Field, value: &Value, siblings: &Map<String, Value>, ctx: &Ctx) -> Option<Cell> {
    if value.is_null() {
        return None;
    }
    let show = field.show.clone().unwrap_or_default();
    let hint = field.show_as();
    match hint {
        ShowAs::Hidden => None,
        ShowAs::Text => {
            let t = scalar_text(value);
            if t.is_empty() {
                return None;
            }
            let lines = show.lines.unwrap_or(if field.ty == FieldType::Text { 3 } else { 1 });
            Some(Cell::Text { text: sanitize_prose(&t).replace('\t', " "), lines })
        }
        ShowAs::Badge => {
            if let Some(b) = value.as_bool() {
                return Some(Cell::Badge { text: if b { "yes" } else { "no" }.into(), glyph: None, tone: if b { Tone::Ok } else { Tone::Dim } });
            }
            let v = value.as_str()?;
            let ev = field.enum_value(v);
            Some(Cell::Badge {
                text: sanitize_line(v).into_owned(),
                glyph: ev.and_then(|e| e.glyph.clone()),
                tone: ev.and_then(|e| e.tone).unwrap_or(Tone::Info),
            })
        }
        ShowAs::Link => {
            let url = value.as_str()?;
            if url.is_empty() {
                return None;
            }
            let host = host_of(url);
            let label = show
                .label_from
                .as_deref()
                .and_then(|k| siblings.get(k))
                .map(scalar_text)
                .filter(|s| !s.is_empty())
                .map(|s| clip(&sanitize_line(&s), 80))
                .unwrap_or_else(|| host.clone());
            Some(Cell::Link { label, url: sanitize_line(url).into_owned(), host: sanitize_line(&host).into_owned(), openable: openable(url) })
        }
        ShowAs::Table => {
            let rows = value.as_array()?;
            let all = resolved_columns(field, Some(ctx.plan));
            let cols: Vec<&Field> = if show.columns.is_empty() {
                all.iter().filter(|c| c.show_as() != ShowAs::Hidden).collect()
            } else {
                show.columns.iter().filter_map(|k| all.iter().find(|c| &c.key == k)).collect()
            };
            let mut objs: Vec<&Map<String, Value>> = rows.iter().filter_map(Value::as_object).collect();
            if let Some(k) = &show.sort {
                objs.sort_by(|a, b| compare_values(a.get(k).unwrap_or(&Value::Null), b.get(k).unwrap_or(&Value::Null)));
            }
            let body: Vec<Vec<Cell>> = objs
                .iter()
                .map(|o| cols.iter().map(|c| o.get(&c.key).and_then(|v| cell_for(c, v, o, ctx)).unwrap_or(Cell::Text { text: String::new(), lines: 1 })).collect())
                .collect();
            Some(Cell::Table { columns: cols.iter().map(|c| Column { key: c.key.clone(), label: c.label_or_key() }).collect(), rows: body })
        }
        ShowAs::Progress => {
            let (frac, text) = match value {
                Value::Object(o) => {
                    let done = o.get("done").and_then(Value::as_f64)?;
                    let total = o.get("total").and_then(Value::as_f64)?;
                    let f = if total > 0.0 { (done / total).clamp(0.0, 1.0) } else { 0.0 };
                    (f, format!("{}/{}", done as i64, total as i64))
                }
                other => {
                    let f = other.as_f64()?.clamp(0.0, 1.0);
                    (f, format!("{:.0}%", f * 100.0))
                }
            };
            Some(Cell::Bar { fraction: frac, text })
        }
        ShowAs::Meter => {
            let v = value.as_f64()?;
            let lo = show.min.unwrap_or(0.0);
            let fraction = show.max.filter(|m| *m > lo).map(|m| ((v - lo) / (m - lo)).clamp(0.0, 1.0));
            let tone = if show.crit_at.is_some_and(|c| v >= c) {
                Tone::Bad
            } else if show.warn_at.is_some_and(|w| v >= w) {
                Tone::Warn
            } else {
                Tone::Ok
            };
            Some(Cell::Meter { fraction, text: number_text(v, show.unit.as_deref()), tone })
        }
        ShowAs::Duration => {
            let ms = value.as_u64().or_else(|| value.as_f64().map(|f| f.max(0.0) as u64))?;
            Some(Cell::Duration { ms, text: format_duration(ms), live: ctx.running })
        }
        ShowAs::Time => {
            let ms = value.as_u64()?;
            Some(Cell::Time { ms, text: format_relative(ms, ctx.now_ms) })
        }
        ShowAs::AgentRef => None,
        ShowAs::Json => Some(Cell::Json(clip(&sanitize_line(&value.to_string()), 200))),
    }
}

fn item_label(form: Option<&Form>, item: &Item, now_ms: u64) -> (String, bool) {
    let Some(tpl) = form.and_then(|f| f.item_label.as_deref()) else {
        let t = if item.label.is_empty() { item.id.clone() } else { item.label.clone() };
        return (clip(&sanitize_line(&t), 160), false);
    };
    let filled = fill_template(tpl, &|name| match name {
        "status" => Some(item.status.as_str().to_string()),
        "kind" => Some(item.kind.clone()),
        "id" => Some(item.id.clone()),
        "label" => Some(item.label.clone()),
        n => {
            if let Some(p) = n.strip_prefix("data.") {
                path_in(&item.data, p).map(scalar_text)
            } else {
                n.strip_prefix("live.").and_then(|k| item.live.get(k, now_ms)).map(|v| scalar_text(&v))
            }
        }
    });
    (clip(&sanitize_line(filled.trim()), 160), tpl.contains("{status}"))
}

fn chips(item: &Item, state: &ProjectState) -> Vec<AgentChip> {
    item.agents
        .iter()
        .map(|id| {
            let a = state.agents.get(id);
            AgentChip { id: id.clone(), role: a.map(|a| sanitize_line(&a.role).into_owned()).unwrap_or_default(), status: a.map(|a| a.status) }
        })
        .collect()
}

/// Resolves one item (and its children) into a [`Row`]. `now_ms` drives live durations and
/// relative times.
pub fn view(item: &Item, state: &ProjectState, now_ms: u64) -> Row {
    let form = state.forms.get(&item.form);
    let ctx = Ctx { plan: &state.plan, now_ms, running: item.status == ItemStatus::Running };
    let (title, status_in_title) = item_label(form, item, now_ms);
    let mut cells = Vec::new();
    match form {
        Some(f) => {
            for field in &f.fields {
                let Ok(src) = field.source() else { continue };
                let value = match src {
                    FieldSource::Data(k) => path_in(&item.data, k).cloned(),
                    FieldSource::Live(k) => item.live.get(k, now_ms),
                    FieldSource::Agents => continue,
                };
                if let Some(cell) = value.and_then(|v| cell_for(field, &v, &item.data, &ctx)) {
                    cells.push(LabeledCell { key: field.key.clone(), label: field.label_or_key(), cell });
                }
            }
        }
        None => {
            // No registered form: draw the data as it is.
            for (k, v) in &item.data {
                let cell = match v {
                    Value::Null => continue,
                    Value::Array(_) | Value::Object(_) => Cell::Json(clip(&sanitize_line(&v.to_string()), 200)),
                    other => Cell::Text { text: sanitize_prose(&scalar_text(other)).into_owned(), lines: 1 },
                };
                cells.push(LabeledCell { key: k.clone(), label: k.replace('_', " "), cell });
            }
        }
    }
    Row {
        id: item.id.clone(),
        kind: item.kind.clone(),
        title,
        status: item.status,
        status_badge: status_badge(item.status),
        status_in_title,
        cells,
        agents: chips(item, state),
        children: state.children(&item.id).into_iter().map(|c| view(c, state, now_ms)).collect(),
    }
}

/// The whole tree: one [`Row`] per root item.
pub fn view_tree(state: &ProjectState, now_ms: u64) -> Vec<Row> {
    state.roots().into_iter().map(|i| view(i, state, now_ms)).collect()
}

fn bar(fraction: f64) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(BAR_WIDTH - filled))
}

/// A cell on one line, as the title line and table cells draw it (no label).
pub fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Text { text, .. } => text.replace('\n', " "),
        Cell::Badge { text, glyph, .. } => match glyph {
            Some(g) => format!("[{text}] {g}"),
            None => format!("[{text}]"),
        },
        Cell::Link { label, host, .. } => {
            if label == host {
                format!("{host} ↗")
            } else {
                format!("{label} ↗ ({host})")
            }
        }
        Cell::Table { rows, .. } => format!("{} rows", rows.len()),
        Cell::Bar { fraction, text } => format!("{text} {}", bar(*fraction)),
        Cell::Meter { fraction, text, .. } => match fraction {
            Some(f) => format!("{} {text}", bar(*f)),
            None => text.clone(),
        },
        Cell::Duration { text, .. } | Cell::Time { text, .. } => text.clone(),
        Cell::Json(s) => s.clone(),
    }
}

fn is_inline(c: &Cell) -> bool {
    matches!(c, Cell::Badge { .. } | Cell::Meter { .. } | Cell::Bar { .. } | Cell::Duration { .. } | Cell::Time { .. })
}

fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        lines.push(cur);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            *last = clip(&format!("{last}..."), width.max(4));
            if !last.ends_with("...") {
                last.push_str("...");
            }
        }
    }
    lines
}

const MAX_TABLE_ROWS: usize = 5;

/// Paints rows as plain text: the title line (title, status, then the inline cells in field
/// order), block cells under it, then the agent chips, children indented. `width` is the column
/// budget for wrapped text. Used by tests and the CLI; the TUI paints [`Row`] itself.
pub fn paint(rows: &[Row], width: usize) -> String {
    let mut out = String::new();
    for r in rows {
        paint_row(r, 0, width, &mut out);
    }
    out
}

fn paint_row(r: &Row, depth: usize, width: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let inner = width.saturating_sub(2 * depth + 2).max(20);
    let mut title = r.title.clone();
    if !r.status_in_title {
        title.push_str("   ");
        title.push_str(r.status.as_str());
    }
    for c in r.cells.iter().filter(|c| is_inline(&c.cell)) {
        title.push_str("   ");
        match &c.cell {
            Cell::Badge { .. } => title.push_str(&cell_text(&c.cell)),
            other => {
                title.push_str(&c.label);
                title.push(' ');
                title.push_str(&cell_text(other));
            }
        }
    }
    out.push_str(&format!("{pad}{title}\n"));
    for c in r.cells.iter().filter(|c| !is_inline(&c.cell)) {
        match &c.cell {
            Cell::Text { text, lines } => {
                for l in wrap(text, inner, (*lines).max(1) as usize) {
                    out.push_str(&format!("{pad}  {l}\n"));
                }
            }
            Cell::Link { .. } | Cell::Json(_) => {
                let t = cell_text(&c.cell);
                if matches!(c.cell, Cell::Json(_)) {
                    out.push_str(&format!("{pad}  {}: {}\n", c.label, t));
                } else {
                    out.push_str(&format!("{pad}  {t}\n"));
                }
            }
            Cell::Table { columns, rows } => {
                if rows.is_empty() {
                    continue;
                }
                out.push_str(&format!("{pad}  {} ({})\n", c.label, rows.len()));
                if columns.len() > 1 {
                    let head: Vec<&str> = columns.iter().map(|c| c.label.as_str()).collect();
                    out.push_str(&format!("{pad}    {}\n", head.join(" | ")));
                }
                for row in rows.iter().take(MAX_TABLE_ROWS) {
                    let cells: Vec<String> = row.iter().map(|c| clip(&cell_text(c), 60)).collect();
                    out.push_str(&format!("{pad}    {}\n", cells.join(" | ")));
                }
                if rows.len() > MAX_TABLE_ROWS {
                    out.push_str(&format!("{pad}    ... +{} more\n", rows.len() - MAX_TABLE_ROWS));
                }
            }
            _ => {}
        }
    }
    if !r.agents.is_empty() {
        let chips: Vec<String> = r.agents.iter().map(|a| if a.role.is_empty() { format!("◆ {}", a.id) } else { format!("◆ {} {}", a.id, a.role) }).collect();
        out.push_str(&format!("{pad}  {}\n", chips.join("   ")));
    }
    for ch in &r.children {
        paint_row(ch, depth + 1, width, out);
    }
}

/// One group of an overview row: a value and how many items hold it.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The value (`High`, `running`, ...).
    pub value: String,
    /// How many.
    pub count: u32,
    /// The tone to draw it in.
    pub tone: Tone,
    /// The glyph, when the value's enum carries one.
    pub glyph: Option<String>,
}

/// One row of the overview: counts of items (or agents) grouped by a field.
#[derive(Debug, Clone, PartialEq)]
pub struct RollupView {
    /// The row title.
    pub title: String,
    /// Everything counted.
    pub total: u32,
    /// The groups.
    pub groups: Vec<Group>,
}

fn status_word_tone(w: &str) -> Tone {
    match w {
        "done" | "upheld" | "supported" => Tone::Ok,
        "running" => Tone::Info,
        "failed" | "refuted" => Tone::Bad,
        "blocked" | "partial" | "contested" => Tone::Warn,
        _ => Tone::Dim,
    }
}

fn item_value(item: &Item, by: &str, now_ms: u64) -> Option<String> {
    match by {
        "status" => Some(item.status.as_str().into()),
        "kind" => Some(item.kind.clone()),
        other => {
            if let Some(p) = other.strip_prefix("data.") {
                path_in(&item.data, p).map(scalar_text).filter(|s| !s.is_empty())
            } else {
                other.strip_prefix("live.").and_then(|k| item.live.get(k, now_ms)).map(|v| scalar_text(&v))
            }
        }
    }
}

fn agent_status_word(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Running => "running",
        AgentStatus::Done => "done",
        AgentStatus::Failed => "failed",
    }
}

fn enum_field<'a>(state: &'a ProjectState, kind: &str, by: &str) -> Option<&'a Field> {
    let key = by.strip_prefix("data.")?;
    let form_id = state.items_of_kind(kind).first().map(|i| i.form.clone())?;
    state.forms.get(&form_id)?.field(key)
}

fn make_groups(counts: BTreeMap<String, u32>, field: Option<&Field>, status_like: bool) -> Vec<Group> {
    let tone_of = |v: &str| -> (Tone, Option<String>) {
        match field.and_then(|f| f.enum_value(v)) {
            Some(e) => (e.tone.unwrap_or(Tone::Info), e.glyph.clone()),
            None if status_like => (status_word_tone(v), None),
            None => (Tone::Info, None),
        }
    };
    let mut groups: Vec<Group> = counts
        .into_iter()
        .map(|(value, count)| {
            let (tone, glyph) = tone_of(&value);
            Group { value, count, tone, glyph }
        })
        .collect();
    match field {
        // Enum order is the form's order (Critical before Low).
        Some(f) if !f.values.is_empty() => {
            groups.sort_by_key(|g| f.values.iter().position(|e| e.v == g.value).unwrap_or(usize::MAX));
        }
        _ => groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value))),
    }
    groups
}

/// The overview rows of one project: the kind's rollups, or, with none, a count of items by kind
/// and status so an unregistered kind still has an overview.
pub fn rollups(state: &ProjectState, template: Option<&KindTemplate>, now_ms: u64) -> Vec<RollupView> {
    let declared: Vec<&Rollup> = template.map(|t| t.rollups.iter().collect()).unwrap_or_default();
    let mut out = Vec::new();
    if declared.is_empty() {
        let mut kinds: Vec<&String> = state.order.keys().collect();
        kinds.sort();
        for kind in kinds {
            let mut counts = BTreeMap::new();
            for i in state.items_of_kind(kind) {
                *counts.entry(i.status.as_str().to_string()).or_insert(0) += 1;
            }
            let total = counts.values().sum();
            out.push(RollupView { title: kind.clone(), total, groups: make_groups(counts, None, true) });
        }
        if !state.agents.is_empty() {
            out.push(agents_rollup("Agents", state, "status"));
        }
        return out;
    }
    for r in declared {
        if r.over == "agents" {
            out.push(agents_rollup(&r.title, state, &r.by));
            continue;
        }
        let mut counts = BTreeMap::new();
        for i in state.items_of_kind(&r.over) {
            let v = item_value(i, &r.by, now_ms).unwrap_or_else(|| "none".into());
            *counts.entry(v).or_insert(0) += 1;
        }
        let total = counts.values().sum();
        let field = enum_field(state, &r.over, &r.by);
        out.push(RollupView { title: r.title.clone(), total, groups: make_groups(counts, field, r.by == "status" || r.by.ends_with("verdict")) });
    }
    out
}

fn agents_rollup(title: &str, state: &ProjectState, by: &str) -> RollupView {
    let mut counts = BTreeMap::new();
    for a in state.agents.values() {
        let v = match by {
            "role" => a.role.clone(),
            "model" => a.model.clone(),
            _ => agent_status_word(a.status).to_string(),
        };
        *counts.entry(v).or_insert(0) += 1;
    }
    let total = counts.values().sum();
    RollupView { title: title.to_string(), total, groups: make_groups(counts, None, true) }
}

/// One project in the overview list.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectLine {
    /// `p_<8hex>`.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its kind.
    pub kind: String,
    /// Its status.
    pub status: ProjectStatus,
    /// The phase it is in (the type name).
    pub phase: String,
    /// The current phase's progress, `done` of `total`.
    pub progress: Progress,
}

/// The overview tab: counts and stats rolled up from every project's JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    /// One line per project, newest first.
    pub projects: Vec<ProjectLine>,
    /// Projects per status, in lifecycle order.
    pub by_status: Vec<(ProjectStatus, u32)>,
    /// The rollups of every project, rows with the same title added together.
    pub rollups: Vec<RollupView>,
}

/// Builds the overview of `states` (any mix of kinds).
pub fn overview(states: &[&ProjectState], registry: &Registry, now_ms: u64) -> Overview {
    let mut projects: Vec<ProjectLine> = states
        .iter()
        .map(|s| {
            let ph = s.current_phase();
            ProjectLine {
                id: s.id.clone(),
                title: sanitize_line(&s.title).into_owned(),
                kind: s.kind.clone(),
                status: s.status,
                phase: ph.map(|p| p.ty.clone()).unwrap_or_default(),
                progress: ph.map(|p| p.progress).unwrap_or_default(),
            }
        })
        .collect();
    let mut created: BTreeMap<&str, u64> = BTreeMap::new();
    for s in states {
        created.insert(&s.id, s.created_ms);
    }
    projects.sort_by(|a, b| created[b.id.as_str()].cmp(&created[a.id.as_str()]).then_with(|| a.id.cmp(&b.id)));
    let order = [
        ProjectStatus::AwaitingPlan,
        ProjectStatus::Running,
        ProjectStatus::AwaitingCheckin,
        ProjectStatus::Finished,
        ProjectStatus::Cancelled,
        ProjectStatus::Failed,
    ];
    let by_status = order.iter().map(|st| (*st, states.iter().filter(|s| s.status == *st).count() as u32)).filter(|(_, n)| *n > 0).collect();
    let mut merged: Vec<RollupView> = Vec::new();
    for s in states {
        for r in rollups(s, registry.kind(&s.kind), now_ms) {
            match merged.iter_mut().find(|m| m.title == r.title) {
                Some(m) => {
                    m.total += r.total;
                    for g in r.groups {
                        match m.groups.iter_mut().find(|x| x.value == g.value) {
                            Some(x) => x.count += g.count,
                            None => m.groups.push(g),
                        }
                    }
                }
                None => merged.push(r),
            }
        }
    }
    Overview { projects, by_status, rollups: merged }
}

/// The overview as plain text (tests and the CLI).
pub fn paint_overview(o: &Overview) -> String {
    let mut out = String::new();
    let statuses: Vec<String> = o.by_status.iter().map(|(s, n)| format!("{n} {}", s.as_str())).collect();
    out.push_str(&format!("{} projects: {}\n", o.projects.len(), statuses.join(", ")));
    for p in &o.projects {
        out.push_str(&format!("  {} {} [{}] {} {}/{}\n", p.id, p.title, p.kind, p.phase, p.progress.done, p.progress.total));
    }
    for r in &o.rollups {
        let groups: Vec<String> = r
            .groups
            .iter()
            .map(|g| match &g.glyph {
                Some(gl) => format!("{} {} {}", g.value, gl, g.count),
                None => format!("{} {}", g.value, g.count),
            })
            .collect();
        out.push_str(&format!("{} {}: {}\n", r.title, r.total, groups.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::form::{Field, Show, Who};
    use crate::mutation::Mutation;
    use crate::state::tests::sample;

    fn analyzed(st: &mut ProjectState) -> String {
        let mut it = Item::new(crate::ids::item_id(&st.id, "source", "k1"), "source", "analyze@1");
        it.parent = Some(st.items_of_kind("question")[0].id.clone());
        it.status = ItemStatus::Done;
        it.live.started_ms = Some(1_000);
        it.live.ended_ms = Some(42_000);
        it.agents = vec!["a_17".into()];
        it.data = json!({
            "index": 3, "title": "Moonshot AI releases Kimi K2.6", "relevancy": "High", "source_kind": "official",
            "one_line": "Kimi K2.6 released 2026-10-20, 1T MoE, 256k context", "source": "https://example.org/kimi-k26",
            "notes": "Official post.", "credibility": 0.72,
            "claims": [{"text": "K2.6 is out", "quote": "K2.6 is out", "sub": "q1"}, {"text": "1T MoE", "quote": "1T MoE", "sub": "q1"}],
            "additional_sources": [
                {"index": 1, "link": 4, "reference": null, "opportunity": "Critical", "notes": "release post", "source": "https://example.org/post"},
                {"index": 2, "link": null, "reference": "Kimi tech report", "opportunity": "Low", "notes": "cited", "source": null}],
            "records": [{"model_name": "Kimi K2.6", "vendor": "Moonshot", "status": "released", "release_date": "2026-10-20", "size": "1T"}]
        })
        .as_object()
        .unwrap()
        .clone();
        let id = it.id.clone();
        st.apply(Mutation::PutAgent(Box::new(Agent {
            id: "a_17".into(), role: "analyst".into(), model: "m".into(), step: "analyze".into(), item: Some(id.clone()),
            status: AgentStatus::Done, attempt: 1, tokens: Tokens::default(), started_ms: 1_000, ended_ms: Some(42_000),
        })), 1).unwrap();
        st.apply(Mutation::PutItem(Box::new(it)), 2).unwrap();
        id
    }

    #[test]
    fn a_research_source_draws_from_its_form_with_no_source_specific_code() {
        let mut st = sample();
        let id = analyzed(&mut st);
        let row = view(st.item(&id).unwrap(), &st, 100_000);
        assert_eq!(row.title, "3. Moonshot AI releases Kimi K2.6");
        let by_key = |k: &str| row.cells.iter().find(|c| c.key == k).map(|c| &c.cell);
        assert!(matches!(by_key("relevancy"), Some(Cell::Badge { text, glyph: Some(g), tone: Tone::Ok }) if text == "High" && g == "●"));
        assert!(matches!(by_key("source"), Some(Cell::Link { label, host, openable: true, .. }) if label == "Moonshot AI releases Kimi K2.6" && host == "example.org"));
        assert!(matches!(by_key("credibility"), Some(Cell::Meter { fraction: Some(f), .. }) if (*f - 0.72).abs() < 1e-9));
        assert!(matches!(by_key("runtime_ms"), Some(Cell::Duration { ms: 41_000, live: false, .. })));
        assert!(matches!(by_key("records"), Some(Cell::Table { rows, .. }) if rows.len() == 1));
        assert!(by_key("index").is_none() && by_key("title").is_none() && by_key("rejected").is_none(), "hidden fields are not drawn");
        assert_eq!(row.agents, [AgentChip { id: "a_17".into(), role: "analyst".into(), status: Some(AgentStatus::Done) }]);
        let text = paint(&[row], 100);
        assert!(text.starts_with("3. Moonshot AI releases Kimi K2.6   done   [High] ●   cred ▰▰▰▰▰▰▱▱ 0.72   [official]   runtime 00:00:41\n"), "{text}");
        assert!(text.contains("  Kimi K2.6 released 2026-10-20, 1T MoE, 256k context\n"));
        assert!(text.contains("  Moonshot AI releases Kimi K2.6 ↗ (example.org)\n"), "{text}");
        assert!(text.contains("  additional sources (2)\n    opportunity | notes | source\n    [Critical] ! | release post | example.org ↗\n"), "{text}");
        assert!(text.contains("  records (1)\n"));
        assert!(text.ends_with("  ◆ a_17 analyst\n"), "{text}");
    }

    fn experiment_state() -> (ProjectState, String) {
        let reg = Registry::builtin();
        let plan = crate::plan::test_plan();
        let mut st = ProjectState::new("p_00000001".into(), plan, &reg, 1);
        st.kind = "engineering".into();
        st.forms = reg.forms_for("engineering");
        let mut it = Item::new("i_e1".into(), "experiment", "experiment@1");
        it.status = ItemStatus::Running;
        it.data = json!({"n": 1, "name": "Trellis Encoder Compression", "overview": "Compresses the trellis encoder activations 4x with a learned codebook; checks loss on the held-out split every 500 steps."})
            .as_object().unwrap().clone();
        it.live = Live { started_ms: Some(1_000_000), cpu_pct: Some(41.0), gpu_pct: Some(78.0), size_bytes: Some(1_288_490_189), ..Live::default() };
        it.agents = vec!["a_31".into(), "a_32".into()];
        st.agents.insert("a_31".into(), Agent { id: "a_31".into(), role: "engineer".into(), model: "m".into(), step: "run".into(), item: Some("i_e1".into()), status: AgentStatus::Running, attempt: 1, tokens: Tokens::default(), started_ms: 1, ended_ms: None });
        st.agents.insert("a_32".into(), Agent { id: "a_32".into(), role: "monitor".into(), model: "m".into(), step: "run".into(), item: Some("i_e1".into()), status: AgentStatus::Running, attempt: 1, tokens: Tokens::default(), started_ms: 1, ended_ms: None });
        st.order.entry("experiment".into()).or_default().push("i_e1".into());
        st.items.insert("i_e1".into(), it);
        (st, "i_e1".into())
    }

    #[test]
    fn an_engineering_experiment_renders_the_design_line_from_its_own_form() {
        let (st, id) = experiment_state();
        // 2,530,000 ms after it started.
        let row = view(st.item(&id).unwrap(), &st, 1_000_000 + 2_530_000);
        let text = paint(&[row], 200);
        let expect = "Experiment #1 (Trellis Encoder Compression) [running]   size 1.2 GB   cpu ▰▰▰▱▱▱▱▱ 41%   gpu ▰▰▰▰▰▰▱▱ 78%   runtime 00:42:10\n  Compresses the trellis encoder activations 4x with a learned codebook; checks loss on the held-out split every 500 steps.\n  ◆ a_31 engineer   ◆ a_32 monitor\n";
        assert_eq!(text, expect);
    }

    #[test]
    fn a_kind_with_only_a_form_and_no_registration_renders() {
        let mut st = sample();
        st.kind = "gizmo".into();
        st.forms.insert(
            "gizmo@1".into(),
            Form {
                form: "gizmo".into(),
                version: 1,
                title: "Gizmo".into(),
                item_label: Some("Gizmo {data.name} ({status})".into()),
                fields: vec![
                    { let mut f = Field::new("name", FieldType::String, Who::Plan); f.max_len = Some(30); f.show = Some(Show::of(ShowAs::Hidden)); f },
                    { let mut f = Field::new("load", FieldType::Number, Who::Code); f.show = Some(Show { as_: ShowAs::Progress, ..Show::default() }); f },
                    { let mut f = Field::new("level", FieldType::Enum, Who::Code); f.values = vec![crate::form::EnumValue { v: "hot".into(), def: None, weight: None, tone: Some(Tone::Bad), glyph: Some("▲".into()) }]; f },
                    { let mut f = Field::new("docs", FieldType::Url, Who::Code); f.label = "docs".into(); f },
                ],
                rules: vec![],
            },
        );
        let mut it = Item::new("i_g1".into(), "gizmo", "gizmo@1");
        it.status = ItemStatus::Running;
        it.data = json!({"name": "Alpha", "load": 0.5, "level": "hot", "docs": "https://example.org/g"}).as_object().unwrap().clone();
        st.order.entry("gizmo".into()).or_default().push("i_g1".into());
        st.items.insert("i_g1".into(), it);
        let row = view(st.item("i_g1").unwrap(), &st, 5);
        let text = paint(&[row], 80);
        assert!(text.starts_with("Gizmo Alpha (running)   load 50% ▰▰▰▰▱▱▱▱   [hot] ▲\n"), "{text}");
        assert!(text.contains("  example.org ↗\n"), "{text}");
    }

    #[test]
    fn an_item_whose_form_is_missing_is_drawn_field_by_field() {
        let mut st = sample();
        let mut it = Item::new("i_x1".into(), "mystery", "mystery@9");
        it.label = "A mystery".into();
        it.data = json!({"a": 1, "b": null, "c": [1, 2], "d": "text"}).as_object().unwrap().clone();
        st.order.entry("mystery".into()).or_default().push("i_x1".into());
        st.items.insert("i_x1".into(), it);
        let row = view(st.item("i_x1").unwrap(), &st, 5);
        assert_eq!(row.title, "A mystery");
        assert_eq!(row.cells.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["a", "c", "d"]);
    }

    #[test]
    fn hostile_text_cannot_drive_the_terminal() {
        let mut st = sample();
        let id = analyzed(&mut st);
        let mut it = st.item(&id).unwrap().clone();
        it.data.insert("title".into(), json!("evil\u{1b}[2K\rtitle\u{202e}"));
        it.data.insert("one_line".into(), json!("line\u{1b}]0;pwned\u{7}"));
        it.data.insert("source".into(), json!("https://exa mple.org/x\u{1b}[31m"));
        st.items.insert(id.clone(), it);
        let row = view(st.item(&id).unwrap(), &st, 5);
        let text = paint(&[row.clone()], 100);
        assert!(!text.chars().any(|c| c == '\u{1b}' || c == '\u{202e}' || c == '\u{7}' || c == '\r'), "{text:?}");
        assert!(text.contains("<U+001B>"));
        assert!(matches!(&row.cells.iter().find(|c| c.key == "source").unwrap().cell, Cell::Link { openable: false, .. }), "a url with a space or an escape cannot be opened");
    }

    #[test]
    fn only_http_and_https_links_can_be_opened() {
        assert!(openable("https://example.org/a?b=c#d"));
        assert!(openable("http://example.org"));
        for bad in ["file:///c:/windows", "javascript:alert(1)", "ftp://example.org", "https://", "example.org", "https://a b.org"] {
            assert!(!openable(bad), "{bad}");
        }
        assert_eq!(host_of("https://user:pw@Example.org:8080/p?q#f"), "Example.org");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_duration(2_530_000), "00:42:10");
        assert_eq!(format_duration(3_661_000), "01:01:01");
        assert_eq!(format_relative(1_000, 1_000 + 5_000), "just now");
        assert_eq!(format_relative(0, 42_000), "42 s ago");
        assert_eq!(format_relative(0, 180_000), "3 min ago");
        assert_eq!(format_relative(0, 7_300_000), "2 h ago");
        assert_eq!(format_relative(200_000_000, 0), "in 2 d");
        assert_eq!(format_bytes(812.0), "812 B");
        assert_eq!(format_bytes(1_288_490_189.0), "1.2 GB");
        assert_eq!(number_text(41.0, Some("%")), "41%");
        assert_eq!(number_text(0.725, None), "0.72");
        assert_eq!(number_text(7.0, Some("ms")), "7 ms");
    }

    #[test]
    fn meters_warn_and_crit_by_threshold_and_progress_reads_both_shapes() {
        let ctx_plan = crate::plan::test_plan();
        let ctx = Ctx { plan: &ctx_plan, now_ms: 0, running: false };
        let mut f = Field::new("cpu", FieldType::Number, Who::Code);
        f.show = Some(Show { as_: ShowAs::Meter, unit: Some("%".into()), min: Some(0.0), max: Some(100.0), warn_at: Some(80.0), crit_at: Some(95.0), ..Show::default() });
        let tone = |v: f64| match cell_for(&f, &json!(v), &Map::new(), &ctx) { Some(Cell::Meter { tone, .. }) => tone, other => panic!("{other:?}") };
        assert_eq!((tone(10.0), tone(80.0), tone(99.0)), (Tone::Ok, Tone::Warn, Tone::Bad));
        let mut p = Field::new("p", FieldType::Json, Who::Code);
        p.show = Some(Show::of(ShowAs::Progress));
        assert!(matches!(cell_for(&p, &json!({"done": 12, "total": 40}), &Map::new(), &ctx), Some(Cell::Bar { fraction, text }) if text == "12/40" && (fraction - 0.3).abs() < 1e-9));
        assert!(matches!(cell_for(&p, &json!(0.25), &Map::new(), &ctx), Some(Cell::Bar { text, .. }) if text == "25%"));
        assert_eq!(bar(0.0), "▱▱▱▱▱▱▱▱");
        assert_eq!(bar(1.0), "▰▰▰▰▰▰▰▰");
    }

    #[test]
    fn long_text_wraps_and_clips_to_its_line_budget() {
        let t = "one two three four five six seven eight nine ten eleven twelve";
        assert_eq!(wrap(t, 20, 10), ["one two three four", "five six seven eight", "nine ten eleven", "twelve"]);
        let clipped = wrap(t, 20, 2);
        assert_eq!(clipped.len(), 2);
        assert!(clipped[1].ends_with("..."), "{clipped:?}");
    }

    #[test]
    fn the_tree_nests_children_under_parents() {
        let mut st = sample();
        let id = analyzed(&mut st);
        let tree = view_tree(&st, 5);
        assert_eq!(tree.len(), 3, "three questions at the top");
        let q1 = &tree[0];
        assert_eq!(q1.children.len(), 1);
        assert_eq!(q1.children[0].id, id);
        let text = paint(&tree, 100);
        assert!(text.contains("\n  3. Moonshot AI releases Kimi K2.6   done"), "{text}");
    }

    #[test]
    fn overview_counts_from_the_json_and_falls_back_for_an_unregistered_kind() {
        let reg = Registry::builtin();
        let mut a = sample();
        analyzed(&mut a);
        let mut b = sample();
        b.id = "p_00000002".into();
        b.created_ms = 2_000;
        b.status = ProjectStatus::Running;
        let o = overview(&[&a, &b], &reg, 10);
        assert_eq!(o.projects.len(), 2);
        assert_eq!(o.projects[0].id, "p_00000002", "newest first");
        assert_eq!(o.by_status, [(ProjectStatus::AwaitingPlan, 1), (ProjectStatus::Running, 1)]);
        let sources = o.rollups.iter().find(|r| r.title == "Sources").unwrap();
        assert_eq!(sources.total, 1);
        assert_eq!(sources.groups[0].value, "High");
        assert_eq!(sources.groups[0].glyph.as_deref(), Some("●"));
        let agents = o.rollups.iter().find(|r| r.title == "Agents").unwrap();
        assert_eq!((agents.total, agents.groups[0].value.as_str()), (1, "done"));
        // An unregistered kind: counted by kind and status.
        let (mut st, _) = experiment_state();
        st.kind = "gizmo".into();
        let o2 = overview(&[&st], &reg, 10);
        let line = paint_overview(&o2);
        assert!(line.contains("experiment 1: running 1"), "{line}");
        assert!(line.contains("question 3: done 3"), "{line}");
        // Two projects of two shapes add into one overview.
        let both = overview(&[&a, &st], &reg, 10);
        assert!(both.rollups.iter().any(|r| r.title == "Sources") && both.rollups.iter().any(|r| r.title == "experiment"));
    }
}
