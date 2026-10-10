//! Jupyter notebooks (design 2.1 T4, T19). Track A.
//!
//! [`render`] turns an `.ipynb` into what `read` returns: every cell with its index, id, type and
//! source, then its outputs (stream text, results, errors with ANSI codes stripped); image
//! outputs come back as bytes for `ResultPart::Image`. [`edit`] is `notebook_edit`: replace,
//! insert or delete one cell, addressed by id or index; the file is written back with the
//! notebook's own indentation and sorted keys, as Jupyter writes it.
//!
//! [`media`] (image headers, base64) and [`pdf`] (text per page) are the other documents `read`
//! renders rather than prints.

pub mod media;
pub mod pdf;

use serde_json::{json, Map, Value};

/// Bytes of one output's text a render keeps.
pub const OUTPUT_TEXT_MAX: usize = 4 * 1024;

/// One image output, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookImage {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The cell index it belongs to.
    pub cell: usize,
}

/// A rendered notebook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub images: Vec<NotebookImage>,
    pub cells: usize,
}

/// Why a notebook could not be read or edited.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotebookError {
    #[error("the file is not a Jupyter notebook: {0}")]
    NotNotebook(String),
    #[error("no cell {0}: {1}")]
    NoCell(String, String),
    #[error("{0}")]
    Invalid(String),
}

pub fn parse(bytes: &[u8]) -> Result<Value, NotebookError> {
    let text = std::str::from_utf8(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
        .map_err(|_| NotebookError::NotNotebook("not UTF-8".into()))?;
    let v: Value = serde_json::from_str(text).map_err(|e| NotebookError::NotNotebook(e.to_string()))?;
    if !v.get("cells").is_some_and(Value::is_array) {
        return Err(NotebookError::NotNotebook("no `cells` array".into()));
    }
    Ok(v)
}

/// A multi-line notebook string (a string, or an array of line strings).
fn joined(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// `s` without ANSI escape sequences (tracebacks are coloured).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn clip(s: &str) -> String {
    if s.len() <= OUTPUT_TEXT_MAX {
        return s.to_string();
    }
    let mut cut = OUTPUT_TEXT_MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…[output truncated, {} more bytes]", &s[..cut], s.len() - cut)
}

/// The notebook's language (`metadata.language_info.name`, else the kernel's, else python).
pub fn language(nb: &Value) -> String {
    nb.pointer("/metadata/language_info/name")
        .or_else(|| nb.pointer("/metadata/kernelspec/language"))
        .and_then(Value::as_str)
        .unwrap_or("python")
        .to_string()
}

/// Every cell with its outputs; image outputs decoded into `images`.
pub fn render(nb: &Value) -> Rendered {
    let cells = nb["cells"].as_array().cloned().unwrap_or_default();
    let lang = language(nb);
    let mut text = String::new();
    let mut images = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let kind = cell.get("cell_type").and_then(Value::as_str).unwrap_or("code");
        let id = cell.get("id").and_then(Value::as_str).map(|s| format!(" id=\"{s}\"")).unwrap_or_default();
        let count = match cell.get("execution_count") {
            Some(Value::Number(n)) => format!(" execution_count=\"{n}\""),
            _ => String::new(),
        };
        let lang_attr = if kind == "code" { format!(" language=\"{lang}\"") } else { String::new() };
        text.push_str(&format!("<cell index=\"{i}\"{id} type=\"{kind}\"{lang_attr}{count}>\n"));
        let src = joined(cell.get("source"));
        text.push_str(src.trim_end_matches('\n'));
        text.push_str("\n</cell>\n");
        let outputs = cell.get("outputs").and_then(Value::as_array).cloned().unwrap_or_default();
        if outputs.is_empty() {
            continue;
        }
        let mut out_text = String::new();
        for o in &outputs {
            match o.get("output_type").and_then(Value::as_str).unwrap_or("") {
                "stream" => out_text.push_str(&clip(&strip_ansi(&joined(o.get("text"))))),
                "error" => {
                    let tb = o.get("traceback").and_then(Value::as_array).map(|a| {
                        a.iter().filter_map(Value::as_str).map(strip_ansi).collect::<Vec<_>>().join("\n")
                    });
                    let head = format!(
                        "{}: {}",
                        o.get("ename").and_then(Value::as_str).unwrap_or("Error"),
                        o.get("evalue").and_then(Value::as_str).unwrap_or("")
                    );
                    out_text.push_str(&clip(&tb.filter(|t| !t.is_empty()).unwrap_or(head)));
                }
                "execute_result" | "display_data" => {
                    let data = o.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
                    let mut shown = false;
                    for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                        let Some(b64) = data.get(mime).map(|v| joined(Some(v))) else { continue };
                        if let Some(bytes) = media::base64_decode(&b64) {
                            if let Some((m, w, h)) = media::sniff_image(&bytes) {
                                out_text.push_str(&format!("[image output: {m}, {w}x{h}, attached]"));
                                images.push(NotebookImage { mime: m, bytes, width: w, height: h, cell: i });
                                shown = true;
                                break;
                            }
                        }
                    }
                    if !shown {
                        let t = data
                            .get("text/plain")
                            .or_else(|| data.get("text/markdown"))
                            .or_else(|| data.get("text/html"))
                            .map(|v| joined(Some(v)));
                        if let Some(t) = t {
                            out_text.push_str(&clip(&strip_ansi(&t)));
                        }
                    }
                }
                _ => {}
            }
            if !out_text.ends_with('\n') {
                out_text.push('\n');
            }
        }
        text.push_str(&format!("<outputs cell=\"{i}\">\n{}</outputs>\n", out_text));
    }
    Rendered { text, images, cells: cells.len() }
}

/// How `notebook_edit` changes the notebook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Replace,
    Insert,
    Delete,
}

impl EditMode {
    pub fn parse(s: &str) -> Option<EditMode> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "replace" | "" => EditMode::Replace,
            "insert" => EditMode::Insert,
            "delete" => EditMode::Delete,
            _ => return None,
        })
    }
}

/// One `notebook_edit` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEdit {
    /// A cell id, or a 0-based index (as a number or digits that are no cell's id). For
    /// `Insert`, the new cell goes after it (`None`: at the top).
    pub cell: Option<String>,
    pub source: String,
    /// `code` or `markdown` (`raw` kept). Required to insert; on replace, changes the type.
    pub cell_type: Option<String>,
    pub mode: EditMode,
}

/// What an edit did: the cell's index after it, and its id if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEdited {
    pub index: usize,
    pub id: Option<String>,
    pub cells: usize,
}

fn find(cells: &[Value], r: &str) -> Option<usize> {
    let r = r.trim();
    if let Some(i) = cells.iter().position(|c| c.get("id").and_then(Value::as_str) == Some(r)) {
        return Some(i);
    }
    let digits = r.strip_prefix("cell-").unwrap_or(r);
    digits.parse::<usize>().ok().filter(|&i| i < cells.len())
}

/// The source as Jupyter stores it: one string per line, terminators kept.
fn source_lines(s: &str) -> Value {
    Value::Array(s.split_inclusive('\n').map(|l| Value::String(l.to_string())).collect())
}

fn new_id(cells: &[Value], seed: &str) -> String {
    let mut n = 0u32;
    loop {
        let h = blake3::hash(format!("{seed}\0{n}\0{}", cells.len()).as_bytes()).to_hex();
        let id = h[..8].to_string();
        if !cells.iter().any(|c| c.get("id").and_then(Value::as_str) == Some(&id)) {
            return id;
        }
        n += 1;
    }
}

fn wants_ids(nb: &Value) -> bool {
    let major = nb.get("nbformat").and_then(Value::as_u64).unwrap_or(4);
    let minor = nb.get("nbformat_minor").and_then(Value::as_u64).unwrap_or(0);
    major > 4 || (major == 4 && minor >= 5)
}

fn set_type(cell: &mut Map<String, Value>, kind: &str) {
    cell.insert("cell_type".into(), Value::String(kind.into()));
    if kind == "code" {
        cell.insert("outputs".into(), json!([]));
        cell.insert("execution_count".into(), Value::Null);
    } else {
        cell.remove("outputs");
        cell.remove("execution_count");
    }
}

/// Applies one cell edit to `nb` in place.
pub fn edit(nb: &mut Value, e: &CellEdit) -> Result<CellEdited, NotebookError> {
    let ids = wants_ids(nb);
    let cells = nb.get_mut("cells").and_then(Value::as_array_mut).ok_or_else(|| NotebookError::NotNotebook("no `cells` array".into()))?;
    let located = match &e.cell {
        Some(r) => Some(find(cells, r).ok_or_else(|| {
            NotebookError::NoCell(r.clone(), format!("the notebook has {} cells (indexes 0-{}) and no cell with that id", cells.len(), cells.len().saturating_sub(1)))
        })?),
        None => None,
    };
    let kind = e.cell_type.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if let Some(k) = kind {
        if !["code", "markdown", "raw"].contains(&k) {
            return Err(NotebookError::Invalid(format!("`cell_type` must be `code` or `markdown`, not `{k}`")));
        }
    }
    match e.mode {
        EditMode::Replace => {
            let i = located.ok_or_else(|| NotebookError::Invalid("`cell` is required to replace a cell".into()))?;
            let cell = cells[i].as_object_mut().ok_or_else(|| NotebookError::Invalid(format!("cell {i} is not an object")))?;
            cell.insert("source".into(), source_lines(&e.source));
            let current = cell.get("cell_type").and_then(Value::as_str).unwrap_or("code").to_string();
            let target = kind.unwrap_or(&current).to_string();
            if target != current || target == "code" {
                set_type(cell, &target);
            }
            let id = cell.get("id").and_then(Value::as_str).map(str::to_string);
            Ok(CellEdited { index: i, id, cells: cells.len() })
        }
        EditMode::Insert => {
            let at = located.map_or(0, |i| i + 1);
            let k = kind.unwrap_or("code");
            let mut cell = Map::new();
            cell.insert("metadata".into(), json!({}));
            cell.insert("source".into(), source_lines(&e.source));
            set_type(&mut cell, k);
            let id = if ids {
                let id = new_id(cells, &e.source);
                cell.insert("id".into(), Value::String(id.clone()));
                Some(id)
            } else {
                None
            };
            cells.insert(at, Value::Object(cell));
            Ok(CellEdited { index: at, id, cells: cells.len() })
        }
        EditMode::Delete => {
            let i = located.ok_or_else(|| NotebookError::Invalid("`cell` is required to delete a cell".into()))?;
            let removed = cells.remove(i);
            let id = removed.get("id").and_then(Value::as_str).map(str::to_string);
            Ok(CellEdited { index: i, id, cells: cells.len() })
        }
    }
}

/// The indentation unit of a JSON file as written (spaces before the first nested key), default 1.
pub fn indent_of(original: &str) -> usize {
    original
        .lines()
        .nth(1)
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .filter(|&n| n > 0 && n <= 8)
        .unwrap_or(1)
}

/// `nb` as Jupyter writes it: sorted keys, `indent` spaces, trailing newline, `eol` endings.
pub fn serialize(nb: &Value, indent: usize, crlf: bool) -> Vec<u8> {
    use serde::Serialize;
    let pad = vec![b' '; indent];
    let mut out = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(&pad);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
    nb.serialize(&mut ser).expect("a Value always serializes");
    out.push(b'\n');
    if crlf {
        let s = String::from_utf8(out).expect("serde_json writes UTF-8");
        return s.replace('\n', "\r\n").into_bytes();
    }
    out
}

#[cfg(test)]
mod tests;
