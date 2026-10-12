//! Writes to a settings file that keep its comments, order and unknown keys (`toml_edit`): add or
//! remove a rule, set a dotted key. Each write goes to a temp file renamed over the original.
//! Claude Code's JSON files are never written.

use std::path::Path;

use marlowe_proto::RuleAction;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::SettingsError;

fn read_doc(path: &Path) -> Result<DocumentMut, SettingsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    text.parse::<DocumentMut>().map_err(|e| SettingsError::File { path: path.display().to_string(), message: e.to_string() })
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<(), SettingsError> {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        return Err(SettingsError::Invalid(format!("{}: Claude Code's settings are read-only to Marlowe", path.display())));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn action_key(a: RuleAction) -> &'static str {
    match a {
        RuleAction::Allow => "allow",
        RuleAction::Ask => "ask",
        RuleAction::Deny => "deny",
    }
}

/// The `[permissions]` table, created if missing.
fn permissions(doc: &mut DocumentMut) -> Result<&mut Table, SettingsError> {
    let item = doc.entry("permissions").or_insert(Item::Table(Table::new()));
    item.as_table_mut().ok_or_else(|| SettingsError::Invalid("`permissions` is not a table".into()))
}

/// Adds `pattern` under `permissions.<action>` (once). The pattern is validated first.
pub fn add_rule(path: &Path, action: RuleAction, pattern: &str) -> Result<bool, SettingsError> {
    let pattern = pattern.trim();
    crate::rules::validate(pattern).map_err(SettingsError::Invalid)?;
    let mut doc = read_doc(path)?;
    let table = permissions(&mut doc)?;
    let list = table.entry(action_key(action)).or_insert(Item::Value(Value::Array(Array::new())));
    let arr = list.as_array_mut().ok_or_else(|| SettingsError::Invalid(format!("`permissions.{}` is not a list", action_key(action))))?;
    if arr.iter().any(|v| v.as_str() == Some(pattern)) {
        return Ok(false);
    }
    arr.push(pattern);
    write_doc(path, &doc)?;
    Ok(true)
}

/// Removes `pattern` from every rule list of the file; whether it was there.
pub fn remove_rule(path: &Path, pattern: &str) -> Result<bool, SettingsError> {
    let pattern = pattern.trim();
    let mut doc = read_doc(path)?;
    let mut removed = false;
    if let Some(table) = doc.get_mut("permissions").and_then(Item::as_table_mut) {
        for key in ["allow", "ask", "deny"] {
            if let Some(arr) = table.get_mut(key).and_then(Item::as_array_mut) {
                let before = arr.len();
                arr.retain(|v| v.as_str() != Some(pattern));
                removed |= arr.len() != before;
            }
        }
    }
    if removed {
        write_doc(path, &doc)?;
    }
    Ok(removed)
}

/// Sets a dotted key (`network.allow_hosts`, `permissions.default_mode`) to a JSON value; `null`
/// removes it. The result must still parse as settings.
pub fn set_key(path: &Path, dotted: &str, value: &serde_json::Value) -> Result<(), SettingsError> {
    let parts: Vec<&str> = dotted.split('.').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(SettingsError::Invalid(format!("`{dotted}` is not a settings key")));
    }
    let mut doc = read_doc(path)?;
    let mut table: &mut Table = doc.as_table_mut();
    for p in &parts[..parts.len() - 1] {
        let item = table.entry(p).or_insert(Item::Table(Table::new()));
        table = item.as_table_mut().ok_or_else(|| SettingsError::Invalid(format!("`{p}` in `{dotted}` is not a table")))?;
    }
    let last = parts[parts.len() - 1];
    if value.is_null() {
        table.remove(last);
    } else {
        table.insert(last, Item::Value(to_toml(value)?));
    }
    let text = doc.to_string();
    crate::file::SettingsFile::from_toml(&text).map_err(|e| SettingsError::Invalid(format!("`{dotted}`: {e}")))?;
    write_doc(path, &doc)
}

fn to_toml(v: &serde_json::Value) -> Result<Value, SettingsError> {
    Ok(match v {
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::from(i),
            (None, Some(f)) => Value::from(f),
            _ => return Err(SettingsError::Invalid(format!("{n} is not a TOML number"))),
        },
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(items) => {
            let mut a = Array::new();
            for i in items {
                a.push(to_toml(i)?);
            }
            Value::Array(a)
        }
        serde_json::Value::Object(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, i) in map {
                t.insert(k, to_toml(i)?);
            }
            Value::InlineTable(t)
        }
        serde_json::Value::Null => return Err(SettingsError::Invalid("null inside a value".into())),
    })
}
