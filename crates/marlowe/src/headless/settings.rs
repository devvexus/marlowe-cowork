//! `--settings` and `--mcp-config` values: a JSON file, inline JSON, or (for settings) a name under
//! `<home>\settings\`.

use std::path::Path;

use marlowe_proto::PermissionMode;
use serde_json::Value;

use super::args::{parse_mode, split_rules};

/// What the run takes from its `--settings`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSettings {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mode: Option<PermissionMode>,
    /// Every other top-level key, forwarded to the daemon as a session-scope setting.
    pub forward: Vec<(String, Value)>,
}

fn read_value(raw: &str, home: &Path, kind: &str) -> Result<Value, String> {
    let text = if raw.trim_start().starts_with('{') {
        raw.to_string()
    } else {
        let direct = Path::new(raw);
        let named = home.join("settings").join(format!("{raw}.json"));
        let path = if direct.is_file() {
            direct.to_path_buf()
        } else if kind == "--settings" && named.is_file() {
            named
        } else {
            return Err(format!("{kind} {raw}: no such file"));
        };
        std::fs::read_to_string(&path).map_err(|e| format!("{kind} {}: {e}", path.display()))?
    };
    // An editor's byte-order mark.
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str(text).map_err(|e| format!("{kind} {raw}: not valid JSON ({e}); TOML settings files are read by the daemon, not by -p"))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).flat_map(split_rules).collect(),
        Some(Value::String(s)) => split_rules(s),
        _ => Vec::new(),
    }
}

/// Merges every `--settings` value, in order.
pub fn load(values: &[String], home: &Path) -> Result<RunSettings, String> {
    let mut out = RunSettings::default();
    for raw in values {
        let v = read_value(raw, home, "--settings")?;
        let obj = v.as_object().ok_or_else(|| format!("--settings {raw}: expected a JSON object"))?;
        // `permissions` first, so its rules precede the flat `allowedTools` keys.
        let mut entries: Vec<(&String, &Value)> = obj.iter().collect();
        entries.sort_by_key(|(k, _)| k.as_str() != "permissions");
        for (key, value) in entries {
            match key.as_str() {
                "permissions" => {
                    out.allow.extend(strings(value.get("allow")));
                    out.deny.extend(strings(value.get("deny")));
                    if let Some(m) = value.get("defaultMode").or_else(|| value.get("default_mode")).and_then(Value::as_str) {
                        out.mode = Some(parse_mode(m)?);
                    }
                }
                "allowedTools" | "allowed_tools" => out.allow.extend(strings(Some(value))),
                "disallowedTools" | "disallowed_tools" => out.deny.extend(strings(Some(value))),
                "permissionMode" | "permission_mode" => out.mode = Some(parse_mode(value.as_str().unwrap_or(""))?),
                _ => out.forward.push((key.clone(), value.clone())),
            }
        }
    }
    Ok(out)
}

/// The `mcpServers` entries of every `--mcp-config` value, `(name, entry)` in order. The file
/// shape is `.mcp.json`'s; a bare map of servers is accepted too.
pub fn mcp_entries(values: &[String], home: &Path) -> Result<Vec<(String, Value)>, String> {
    let mut out = Vec::new();
    for raw in values {
        let v = read_value(raw, home, "--mcp-config")?;
        let servers = v.get("mcpServers").unwrap_or(&v);
        let map = servers.as_object().ok_or_else(|| format!("--mcp-config {raw}: expected an object of servers"))?;
        for (name, entry) in map {
            if !entry.is_object() {
                return Err(format!("--mcp-config {raw}: server {name:?} must be an object"));
            }
            out.push((name.clone(), entry.clone()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-hl-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn permissions_become_rules_and_a_mode_and_the_rest_is_forwarded() {
        let home = tmp("a");
        let raw = json!({
            "permissions": {"allow": ["Bash(git diff:*)", "Read"], "deny": ["Bash(rm:*)"], "defaultMode": "acceptEdits"},
            "allowedTools": "Glob",
            "network": {"allow_hosts": ["x.com"]},
        })
        .to_string();
        let s = load(&[raw], &home).unwrap();
        assert_eq!(s.allow, ["Bash(git diff:*)", "Read", "Glob"]);
        assert_eq!(s.deny, ["Bash(rm:*)"]);
        assert_eq!(s.mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(s.forward, [("network".to_string(), json!({"allow_hosts": ["x.com"]}))]);
    }

    #[test]
    fn a_file_and_a_named_profile_load_and_a_missing_one_says_so() {
        let home = tmp("b");
        std::fs::write(home.join("s.json"), "\u{feff}{\"permissions\":{\"deny\":[\"WebFetch\"]}}").unwrap();
        let s = load(&[home.join("s.json").display().to_string()], &home).unwrap();
        assert_eq!(s.deny, ["WebFetch"]);
        std::fs::create_dir_all(home.join("settings")).unwrap();
        std::fs::write(home.join("settings").join("eval.json"), r#"{"permissions":{"allow":["Edit"]}}"#).unwrap();
        assert_eq!(load(&["eval".into()], &home).unwrap().allow, ["Edit"]);
        assert!(load(&["nope".into()], &home).unwrap_err().contains("no such file"));
        assert!(load(&["{not json".into()], &home).unwrap_err().contains("not valid JSON"));
        assert!(load(&["[1]".into()], &home).is_err());
        assert!(load(&[r#"{"permissions":{"defaultMode":"yolo"}}"#.into()], &home).is_err());
    }

    #[test]
    fn mcp_config_reads_mcp_servers_or_a_bare_map() {
        let home = tmp("c");
        let a = json!({"mcpServers": {"fs": {"command": "x"}}}).to_string();
        let b = json!({"web": {"type": "http", "url": "http://127.0.0.1:1/mcp"}}).to_string();
        let got = mcp_entries(&[a, b], &home).unwrap();
        assert_eq!(got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["fs", "web"]);
        assert!(mcp_entries(&[r#"{"mcpServers":{"x":3}}"#.into()], &home).is_err());
    }
}
