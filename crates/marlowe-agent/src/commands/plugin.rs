//! Local plugin bundles (design 2.6 X17), Claude Code's layout: a directory with
//! `.claude-plugin/plugin.json` (or `.marlowe-plugin/plugin.json`) and any of `commands/`,
//! `agents/`, `skills/`, `output-styles/`, `hooks/hooks.json` and `.mcp.json`. The manifest may
//! add paths (`commands`, `agents`, `skills`, `outputStyles`: a string or a list) and give
//! `hooks` / `mcpServers` inline or as a path. A bundle's commands, agents and skills are named
//! `<plugin>:<name>`; `${CLAUDE_PLUGIN_ROOT}` (and `${MARLOWE_PLUGIN_ROOT}`) expand to its root.
//! Hooks and MCP servers are parsed here and handed to their owners (hooks: track E; MCP: K).
//! Marketplaces are not supported (design 9.3-b): bundles load from local paths only.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::instructions::Sources;

/// One loaded bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    pub root: PathBuf,
    pub command_paths: Vec<PathBuf>,
    pub agent_paths: Vec<PathBuf>,
    pub skill_paths: Vec<PathBuf>,
    pub output_style_paths: Vec<PathBuf>,
    /// The hooks configuration (`{"hooks": {...}}` or the bare event map), with the plugin root
    /// expanded in every command.
    pub hooks: Option<Value>,
    /// MCP server definitions (`{"name": {...}}`), with the plugin root expanded.
    pub mcp_servers: Option<Value>,
    pub warnings: Vec<String>,
}

const MANIFESTS: [&str; 3] = [".claude-plugin/plugin.json", ".marlowe-plugin/plugin.json", "plugin.json"];

/// Replaces `${CLAUDE_PLUGIN_ROOT}` / `${MARLOWE_PLUGIN_ROOT}` with `root`.
pub fn expand_root(text: &str, root: &Path) -> String {
    let r = root.display().to_string();
    text.replace("${CLAUDE_PLUGIN_ROOT}", &r).replace("${MARLOWE_PLUGIN_ROOT}", &r)
}

fn expand_json(v: &Value, root: &Path) -> Value {
    match v {
        Value::String(s) => Value::String(expand_root(s, root)),
        Value::Array(a) => Value::Array(a.iter().map(|x| expand_json(x, root)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), expand_json(x, root))).collect()),
        other => other.clone(),
    }
}

fn paths_field(m: &Value, key: &str, root: &Path) -> Vec<PathBuf> {
    let list: Vec<&str> = match m.get(key) {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    list.into_iter().map(|p| crate::instructions::normalize(&root.join(p))).collect()
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("{}: {e}", path.display()))
}

/// Loads the bundle at `root`. A directory with no manifest still loads when it has any of the
/// standard subdirectories (named after the directory).
pub fn load(root: &Path) -> Result<Plugin, String> {
    let manifest_path = MANIFESTS.iter().map(|m| root.join(m)).find(|p| p.is_file());
    let manifest = match &manifest_path {
        Some(p) => read_json(p)?,
        None => {
            let any = ["commands", "agents", "skills", "hooks", "output-styles"].iter().any(|d| root.join(d).is_dir());
            if !any {
                return Err(format!("{} has no .claude-plugin/plugin.json", root.display()));
            }
            Value::Object(Default::default())
        }
    };
    let dir_name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "plugin".into());
    let name = manifest.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or(dir_name);
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains(':') {
        return Err(format!("{}: plugin name `{name}` must be kebab-case without spaces or colons", root.display()));
    }
    let mut warnings = Vec::new();
    let with_default = |key: &str, default: &str| {
        let mut v = vec![root.join(default)];
        v.extend(paths_field(&manifest, key, root));
        v.retain(|p| p.exists());
        v.dedup();
        v
    };
    let command_paths = with_default("commands", "commands");
    let agent_paths = with_default("agents", "agents");
    let skill_paths = with_default("skills", "skills");
    let output_style_paths = with_default("outputStyles", "output-styles");

    let hooks = match manifest.get("hooks") {
        Some(Value::String(p)) => match read_json(&root.join(p)) {
            Ok(v) => Some(v),
            Err(e) => {
                warnings.push(format!("hooks: {e}"));
                None
            }
        },
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => {
            let p = root.join("hooks").join("hooks.json");
            p.is_file().then(|| read_json(&p)).transpose().unwrap_or_else(|e| {
                warnings.push(format!("hooks: {e}"));
                None
            })
        }
    }
    .map(|v| expand_json(&v, root));

    let mcp_servers = match manifest.get("mcpServers") {
        Some(Value::String(p)) => read_json(&root.join(p)).map_err(|e| warnings.push(format!("mcpServers: {e}"))).ok(),
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => {
            let p = root.join(".mcp.json");
            p.is_file().then(|| read_json(&p)).transpose().unwrap_or_else(|e| {
                warnings.push(format!(".mcp.json: {e}"));
                None
            })
        }
    }
    .map(|v| {
        // `.mcp.json` wraps servers in `mcpServers`; a plugin's may be the bare map.
        let v = v.get("mcpServers").cloned().unwrap_or(v);
        expand_json(&v, root)
    });

    Ok(Plugin {
        name,
        version: manifest.get("version").and_then(Value::as_str).map(str::to_string),
        description: manifest.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        root: root.to_path_buf(),
        command_paths,
        agent_paths,
        skill_paths,
        output_style_paths,
        hooks,
        mcp_servers,
        warnings,
    })
}

/// Bundles for a session: every subdirectory of `<home>/plugins` and of each root's
/// `.marlowe/plugins`, plus the explicit `plugin_roots`. A bundle that fails to load is a
/// warning; a second bundle with a name already loaded is skipped with a warning.
pub fn discover(src: &Sources) -> (Vec<Plugin>, Vec<String>) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut scan = |dir: PathBuf| {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            v.sort();
            candidates.extend(v);
        }
    };
    if let Some(h) = &src.home {
        scan(h.join("plugins"));
    }
    for r in &src.roots {
        scan(r.join(".marlowe").join("plugins"));
    }
    candidates.extend(src.plugin_roots.iter().cloned());
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut warnings = Vec::new();
    for c in candidates {
        match load(&c) {
            Ok(p) if plugins.iter().any(|q| q.name == p.name) => {
                warnings.push(format!("plugin `{}` at {} skipped: a plugin of that name is already loaded", p.name, c.display()));
            }
            Ok(p) => {
                warnings.extend(p.warnings.iter().map(|w| format!("plugin `{}`: {w}", p.name)));
                plugins.push(p);
            }
            Err(e) => warnings.push(format!("plugin not loaded: {e}")),
        }
    }
    (plugins, warnings)
}
