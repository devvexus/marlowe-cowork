//! `<home>\export.json`: which projects opted in (none by default), extra terms to redact, and
//! terms the pattern layers must leave alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "export.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportConfig {
    /// Project roots whose sessions and crew runs may be exported. Empty: export is off.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Always redacted (case-insensitive): codenames, client names, a colleague's name.
    #[serde(default)]
    pub redact_terms: Vec<String>,
    /// Never redacted by the credential, personal-data or entropy layers (false positives).
    #[serde(default)]
    pub allow_terms: Vec<String>,
}

pub fn path(home: &Path) -> PathBuf {
    home.join(CONFIG_FILE)
}

impl ExportConfig {
    /// The config, or the default (nothing opted in) when the file is absent.
    pub fn load(home: &Path) -> Result<ExportConfig, String> {
        let p = path(home);
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ExportConfig::default()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }

    /// Writes the config atomically (temp file + rename).
    pub fn save(&self, home: &Path) -> Result<(), String> {
        std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
        let p = path(home);
        let tmp = p.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
    }

    /// The opted-in project that contains `workspace` (the deepest one), if any.
    pub fn project_of(&self, workspace: &str) -> Option<&str> {
        let ws = norm_path(workspace);
        self.projects
            .iter()
            .filter(|p| {
                let p = norm_path(p);
                !p.is_empty() && (ws == p || ws.starts_with(&format!("{p}\\")))
            })
            .max_by_key(|p| p.len())
            .map(String::as_str)
    }
}

/// Comparable form of a path: backslashes, no `\\?\` prefix, no trailing separator, lower-case
/// on Windows (paths there are case-insensitive).
pub fn norm_path(p: &str) -> String {
    let p = p.trim().replace('/', "\\");
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p);
    let p = p.trim_end_matches('\\');
    if cfg!(windows) {
        p.to_lowercase()
    } else {
        p.to_string()
    }
}

/// An existing directory's absolute form, without the `\\?\` prefix `canonicalize` adds.
pub fn canonical_dir(dir: &str) -> Result<String, String> {
    let c = std::fs::canonicalize(dir).map_err(|e| format!("{dir}: {e}"))?;
    if !c.is_dir() {
        return Err(format!("{dir} is not a directory"));
    }
    let s = c.to_string_lossy().to_string();
    Ok(s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s))
}
