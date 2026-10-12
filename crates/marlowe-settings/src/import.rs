//! Read-only import of Claude Code's `settings.json` (design 9.2-e): `permissions`
//! (`allow`/`ask`/`deny`/`defaultMode`/`additionalDirectories`), `hooks` (same shape) and
//! `disableAllHooks`. Rule spellings (`Bash(npm test:*)`, `WebFetch(domain:x)`) are parsed by
//! `marlowe-permission`'s rules as they are. `env`, `model`, `apiKeyHelper` and the rest are not
//! imported: they would run or reconfigure things the import cannot vouch for. Marlowe never
//! writes these files.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::file::{MatcherGroup, Network, Permissions, SettingsFile};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSettings {
    #[serde(default)]
    permissions: ClaudePermissions,
    #[serde(default)]
    hooks: BTreeMap<String, Vec<MatcherGroup>>,
    #[serde(default)]
    disable_all_hooks: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudePermissions {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    default_mode: Option<String>,
    #[serde(default)]
    additional_directories: Vec<String>,
}

/// Claude Code's mode names -> Marlowe's.
pub fn mode_name(cc: &str) -> Option<&'static str> {
    Some(match cc {
        "default" => "default",
        "acceptEdits" | "accept_edits" => "accept_edits",
        "plan" => "plan",
        "bypassPermissions" | "bypass" | "auto" => "bypass",
        "dontAsk" | "dont_ask" => "dont_ask",
        _ => return None,
    })
}

/// Parses a Claude Code settings.json into the Marlowe schema.
pub fn from_claude_json(text: &str) -> Result<SettingsFile, String> {
    let cc: ClaudeSettings = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(SettingsFile {
        permissions: Permissions {
            default_mode: cc.permissions.default_mode.as_deref().and_then(mode_name).map(str::to_string),
            allow: cc.permissions.allow,
            ask: cc.permissions.ask,
            deny: cc.permissions.deny,
            additional_directories: cc.permissions.additional_directories,
            ..Permissions::default()
        },
        network: Network::default(),
        hooks: cc.hooks,
        disable_all_hooks: cc.disable_all_hooks,
        import: Default::default(),
    })
}

/// A `--settings` argument: inline JSON (Claude Code's form) or TOML, or a path to either.
pub fn from_cli_arg(arg: &str) -> Result<SettingsFile, String> {
    let trimmed = arg.trim();
    if trimmed.starts_with('{') {
        return from_claude_json(trimmed);
    }
    let path = std::path::Path::new(trimmed);
    let text = std::fs::read_to_string(path).map_err(|e| format!("{trimmed}: {e}"))?;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        from_claude_json(&text)
    } else {
        SettingsFile::from_toml(&text)
    }
}
