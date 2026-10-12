//! Saved and named workflows (WF6): where a name resolves, in order —
//!
//! 1. the project's `.marlowe\workflows\<name>.star`,
//! 2. the user's `<home>\workflows\<name>.star`,
//! 3. plugin bundles' `<plugin>\workflows\<name>.star` (X17),
//! 4. the built-ins (`code-review`, `simplify`; `workflows/` in this repository).
//!
//! Claude Code's `.claude\workflows\*.js` are listed read-only (9.2-q) with a note to convert
//! them once (`/workflow convert <name>` writes `.marlowe\workflows\<name>.star`); running one by
//! name says so instead of failing obscurely.

use std::path::{Path, PathBuf};

use marlowe_proto::SavedWorkflow;

use crate::validate::{self, Meta};

/// The built-in workflows: name and source.
pub const BUILTINS: [(&str, &str); 2] = [
    ("code-review", include_str!("../../../workflows/code-review.star")),
    ("simplify", include_str!("../../../workflows/simplify.star")),
];

/// The source of the built-in `name`.
pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// Where a workflow was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Project(PathBuf),
    User(PathBuf),
    Plugin(PathBuf),
    Builtin,
    /// A Claude Code JavaScript workflow in `.claude\workflows`: listed, never run as is.
    ClaudeJs(PathBuf),
}

impl Origin {
    /// What `SavedWorkflow::source` shows: the file, or `builtin`.
    pub fn source(&self) -> String {
        match self {
            Origin::Project(p) | Origin::User(p) | Origin::Plugin(p) | Origin::ClaudeJs(p) => p.display().to_string(),
            Origin::Builtin => "builtin".into(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Origin::Project(p) | Origin::User(p) | Origin::Plugin(p) | Origin::ClaudeJs(p) => Some(p),
            Origin::Builtin => None,
        }
    }
}

/// A resolved workflow.
#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub origin: Origin,
    pub source: String,
}

impl Found {
    /// The file name errors show (`code-review.star`).
    pub fn filename(&self) -> String {
        match self.origin.path() {
            Some(p) => p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| format!("{}.star", self.name)),
            None => format!("{}.star", self.name),
        }
    }
}

/// Where to look.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    /// Workspace roots (each may have `.marlowe\workflows` and `.claude\workflows`).
    pub workspaces: Vec<PathBuf>,
    /// The profile home (`<home>\workflows`).
    pub home: Option<PathBuf>,
    /// Plugin bundle roots.
    pub plugins: Vec<PathBuf>,
}

/// A saved workflow's file name is its name: letters, digits, `-`, `_`, `.`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && !name.starts_with('.')
}

fn star_in(dir: &Path, name: &str) -> Option<(PathBuf, String)> {
    let p = dir.join(format!("{name}.star"));
    std::fs::read_to_string(&p).ok().map(|s| (p, s))
}

impl Roots {
    /// Finds `name` in priority order.
    pub fn resolve(&self, name: &str) -> Result<Found, String> {
        let name = name.trim();
        if !valid_name(name) {
            return Err(format!("`{name}` is not a workflow name (letters, digits, -, _, .)"));
        }
        for ws in &self.workspaces {
            if let Some((p, s)) = star_in(&ws.join(".marlowe").join("workflows"), name) {
                return Ok(Found { name: name.into(), origin: Origin::Project(p), source: s });
            }
        }
        if let Some(home) = &self.home {
            if let Some((p, s)) = star_in(&home.join("workflows"), name) {
                return Ok(Found { name: name.into(), origin: Origin::User(p), source: s });
            }
        }
        for root in &self.plugins {
            if let Some((p, s)) = star_in(&root.join("workflows"), name) {
                return Ok(Found { name: name.into(), origin: Origin::Plugin(p), source: s });
            }
        }
        if let Some(s) = builtin(name) {
            return Ok(Found { name: name.into(), origin: Origin::Builtin, source: s.to_string() });
        }
        for ws in &self.workspaces {
            let js = ws.join(".claude").join("workflows").join(format!("{name}.js"));
            if js.is_file() {
                return Err(format!(
                    "`{name}` is a Claude Code JavaScript workflow ({}). Workflows here are Starlark: convert it once with `/workflow convert {name}` (it writes .marlowe/workflows/{name}.star and checks it), then run it by name.",
                    js.display()
                ));
            }
        }
        Err(format!("no workflow named `{name}`; `/workflows` lists them"))
    }

    /// Every workflow, first by priority wins per name, then the JavaScript ones not yet converted.
    pub fn list(&self) -> Vec<(SavedWorkflow, Origin)> {
        let mut out: Vec<(SavedWorkflow, Origin)> = Vec::new();
        let mut push = |name: String, origin: Origin, source: &str| {
            if out.iter().any(|(s, _)| s.name == name) {
                return;
            }
            let meta = meta_of(&name, source);
            out.push((
                SavedWorkflow {
                    name,
                    description: meta.description,
                    when_to_use: meta.when_to_use,
                    phases: meta.phases,
                    source: origin.source(),
                },
                origin,
            ));
        };
        let stars = |dir: PathBuf| -> Vec<(String, PathBuf, String)> {
            let mut v: Vec<(String, PathBuf, String)> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "star"))
                        .filter_map(|p| {
                            let n = p.file_stem()?.to_string_lossy().into_owned();
                            let s = std::fs::read_to_string(&p).ok()?;
                            valid_name(&n).then_some((n, p, s))
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        for ws in &self.workspaces {
            for (n, p, s) in stars(ws.join(".marlowe").join("workflows")) {
                push(n, Origin::Project(p), &s);
            }
        }
        if let Some(home) = &self.home {
            for (n, p, s) in stars(home.join("workflows")) {
                push(n, Origin::User(p), &s);
            }
        }
        for root in &self.plugins {
            for (n, p, s) in stars(root.join("workflows")) {
                push(n, Origin::Plugin(p), &s);
            }
        }
        for (n, s) in BUILTINS {
            push(n.to_string(), Origin::Builtin, s);
        }
        for ws in &self.workspaces {
            let dir = ws.join(".claude").join("workflows");
            let mut js: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "js")).collect())
                .unwrap_or_default();
            js.sort();
            for p in js {
                let Some(n) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
                if !valid_name(&n) || out.iter().any(|(s, _)| s.name == n) {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                let meta = crate::convert::js_meta(&text).and_then(|m| Meta::from_json(&m).ok()).unwrap_or_default();
                out.push((
                    SavedWorkflow {
                        name: n.clone(),
                        description: format!(
                            "{} (Claude Code JavaScript; convert once with /workflow convert {n})",
                            if meta.description.is_empty() { "a CC workflow".to_string() } else { meta.description }
                        ),
                        when_to_use: meta.when_to_use,
                        phases: meta.phases,
                        source: p.display().to_string(),
                    },
                    Origin::ClaudeJs(p),
                ));
            }
        }
        out
    }
}

fn meta_of(name: &str, source: &str) -> Meta {
    match validate::static_checks(&format!("{name}.star"), source) {
        Ok(v) => v.meta,
        Err(d) => Meta { description: format!("(invalid: {})", d.first().map(|d| d.to_string()).unwrap_or_default()), ..Meta::default() },
    }
}

/// The saved-workflow lines for the skills index (3.4): `name: description (when to use)`,
/// within `budget` characters; the rest are counted, not dropped silently.
pub fn index(list: &[SavedWorkflow], budget: usize) -> String {
    let mut out = String::new();
    let mut left = 0usize;
    for w in list {
        let mut line = format!("- {}: {}", w.name, w.description.trim());
        if let Some(u) = w.when_to_use.as_deref().filter(|u| !u.trim().is_empty()) {
            line.push_str(&format!(" Use when: {}", u.trim()));
        }
        line.push('\n');
        if out.len() + line.len() > budget {
            left += 1;
            continue;
        }
        out.push_str(&line);
    }
    if left > 0 {
        out.push_str(&format!("- ({left} more: /workflows lists them)\n"));
    }
    out
}

/// A child a running workflow starts with `workflow(name)`: a saved name, or a `.star` path (a
/// relative one is in the workspace). The parent's approval showed only the parent, so a child runs
/// only when it is built in or its digest was approved on its own.
impl crate::engine::ScriptResolver for Roots {
    fn resolve(&self, name_or_path: &str) -> Result<(String, String), String> {
        let p = Path::new(name_or_path);
        let (file, source, builtin) = if name_or_path.ends_with(".star") && (p.is_absolute() || name_or_path.contains(['/', '\\'])) {
            if !p.is_absolute() && self.workspaces.is_empty() {
                return Err(format!("{name_or_path} is a relative path and this run has no workspace; give a saved name or an absolute path"));
            }
            let path = crate::service::resolve_path(self, name_or_path);
            let s = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| name_or_path.to_string());
            (file, s, false)
        } else {
            let f = Roots::resolve(self, name_or_path)?;
            let builtin = matches!(f.origin, Origin::Builtin);
            (f.filename(), f.source, builtin)
        };
        let approved = builtin || self.home.as_deref().is_some_and(|h| crate::service::digest_trusted(h, &crate::digest(&source)));
        if !approved {
            return Err(format!(
                "{name_or_path} has not been approved to run (this version of it never ran on its own); start it once by itself to approve it, then run this workflow again"
            ));
        }
        Ok((file, source))
    }
}
