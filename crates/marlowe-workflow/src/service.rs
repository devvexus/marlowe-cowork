//! What the `workflow` tool (marlowe-exec) needs from the daemon, and the pieces both sides
//! share: resolving what the call names into a checked script, and the trust store (the first run
//! of a script digest asks; built-ins are trusted).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_model::CancelToken;
use marlowe_proto::SessionId;
use serde_json::Value;

use crate::saved::{self, Origin, Roots};
use crate::validate::{self, Meta};

/// A `workflow` call, as the daemon's launcher takes it.
#[derive(Debug, Clone, Default)]
pub struct LaunchRequest {
    pub session: Option<SessionId>,
    pub workspace: Option<PathBuf>,
    /// An inline script.
    pub script: Option<String>,
    /// A script on disk.
    pub script_path: Option<String>,
    /// A saved workflow.
    pub name: Option<String>,
    /// The JSON the script gets, verbatim.
    pub args: Value,
    /// Return at once (true, the default) or wait for the result (false).
    pub background: bool,
    /// Seed from this run's journal (WF5).
    pub resume_from: Option<String>,
    /// The calling conversation had read untrusted content.
    pub tainted: bool,
    /// The digest the user approved (or that was trusted) when the call was checked. An untrusted
    /// script runs only when it still has this digest, and only this digest is recorded as trusted.
    pub approved_digest: Option<String>,
}

/// A started run.
#[derive(Debug, Clone, PartialEq)]
pub struct Launched {
    pub run_id: String,
    /// Where the script is (an inline one was persisted here).
    pub script_path: String,
    pub name: String,
    pub meta: Meta,
}

/// How a waited-on run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Waited {
    /// The script returned (bounded for the tool result; the journal has it whole).
    Done { value: Value, tainted: bool },
    Failed(String),
    /// The caller stopped waiting (Esc); the run goes on in the background.
    Detached,
}

/// The daemon side of the `workflow` tool.
pub trait WorkflowLauncher: Send + Sync {
    /// Checks and starts a run; never waits for it. `Err` is for the model (bad script, unknown
    /// name, no crew) and nothing started.
    fn launch(&self, req: LaunchRequest) -> Result<Launched, String>;

    /// Waits for a run this launcher started, until it ends or `cancel` fires.
    fn wait(&self, run: &str, cancel: &CancelToken) -> Waited;
}

/// The launcher as a session service (`Services::get::<Workflows>()`).
#[derive(Clone)]
pub struct Workflows(pub Arc<dyn WorkflowLauncher>);

impl std::fmt::Debug for Workflows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Team tasks")
    }
}

/// Where `report_findings` announces what it stored (the daemon broadcasts `Event::Findings`).
#[derive(Clone)]
pub struct FindingsSink(pub Arc<dyn Fn(&SessionId, &[marlowe_proto::Finding]) + Send + Sync>);

impl std::fmt::Debug for FindingsSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FindingsSink")
    }
}

/// A script ready to run: its text, where it came from, and its checked META.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub name: String,
    pub filename: String,
    pub source: String,
    /// The file it was read from; `None` for an inline script (persisted at launch) or a built-in.
    pub path: Option<PathBuf>,
    pub builtin: bool,
    pub meta: Meta,
    pub digest: String,
}

/// Resolves exactly one of `script`, `script_path`, `name` and checks it completely
/// ([`crate::check`]). The error lists every problem, for the model.
pub fn prepare(roots: &Roots, script: Option<&str>, script_path: Option<&str>, name: Option<&str>) -> Result<Prepared, String> {
    let given = [script.is_some(), script_path.is_some(), name.is_some()].iter().filter(|b| **b).count();
    if given != 1 {
        return Err("give exactly one of `script` (inline Starlark), `script_path` (a .star file) or `name` (a saved team task)".into());
    }
    let (name, filename, source, path, builtin) = if let Some(s) = script {
        ("inline".to_string(), "inline.star".to_string(), s.to_string(), None, false)
    } else if let Some(p) = script_path {
        let path = resolve_path(roots, p);
        if path.extension().is_some_and(|x| x == "js") {
            return Err(format!(
                "{} is JavaScript. Team tasks here are Starlark: convert it once with `/team convert <name>`, or write the .star yourself",
                path.display()
            ));
        }
        let source = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "workflow".into());
        let file = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string());
        (stem, file, source, Some(path), false)
    } else {
        let found = roots.resolve(name.unwrap_or_default())?;
        let builtin = found.origin == Origin::Builtin;
        let file = found.filename();
        (found.name.clone(), file, found.source.clone(), found.origin.path().map(Path::to_path_buf), builtin)
    };
    let checked = crate::check(&filename, &source).map_err(|d| format!("{filename} is not valid; nothing ran. Fix these and call again:\n{}", validate::render(&d)))?;
    let name = checked.meta.name.clone().filter(|_| script.is_some()).unwrap_or(name);
    Ok(Prepared { digest: crate::digest(&source), name, filename, source, path, builtin, meta: checked.meta })
}

/// A relative `script_path` is taken against the first workspace root.
pub fn resolve_path(roots: &Roots, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        return path;
    }
    match roots.workspaces.first() {
        Some(ws) => ws.join(path),
        None => path,
    }
}

/// `<home>\workflows\trusted.json`: digests the user approved, with the name each ran as.
pub fn trusted_path(home: &Path) -> PathBuf {
    home.join("workflows").join("trusted.json")
}

fn trusted_map(home: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(trusted_path(home)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// A digest the user approved before.
pub fn digest_trusted(home: &Path, digest: &str) -> bool {
    trusted_map(home).contains_key(digest)
}

/// The digest ran before with the user's approval (built-ins always count).
pub fn is_trusted(home: &Path, prepared: &Prepared) -> bool {
    prepared.builtin || trusted_map(home).contains_key(&prepared.digest)
}

/// Whether `prepared` may run: trusted, or exactly the digest that was approved.
pub fn may_run(home: &Path, prepared: &Prepared, approved_digest: Option<&str>) -> Result<(), String> {
    if is_trusted(home, prepared) || approved_digest == Some(prepared.digest.as_str()) {
        return Ok(());
    }
    Err(format!(
        "{} is not the script that was checked for approval (it changed, or appeared, after the check); nothing ran. Call `team` again to see and approve this version",
        prepared.filename
    ))
}

/// After a launch: records the digest when it is the one the user approved (never a script that
/// only ran because it was already trusted, and never one nobody saw).
pub fn trust_approved(home: &Path, prepared: &Prepared, approved_digest: Option<&str>) -> std::io::Result<()> {
    if approved_digest == Some(prepared.digest.as_str()) {
        trust(home, prepared)?;
    }
    Ok(())
}

/// Records an approved digest.
pub fn trust(home: &Path, prepared: &Prepared) -> std::io::Result<()> {
    if prepared.builtin {
        return Ok(());
    }
    let mut m = trusted_map(home);
    if m.insert(prepared.digest.clone(), prepared.name.clone()).is_none() {
        let p = trusted_path(home);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_string_pretty(&m).map_err(std::io::Error::other)?)?;
    }
    Ok(())
}

/// Arguments a built-in remembers per project until the user says `default` (G11: the
/// code-review level and finding cap).
const REMEMBERED: [(&str, &[&str]); 1] = [("code-review", &["level", "max_findings"])];

fn prefs_path(home: &Path) -> PathBuf {
    home.join("workflows").join("prefs.json")
}

/// Fills `args` with what `name` remembers for `project`, and remembers what the user set now
/// (`"default"` forgets it and leaves the script's default).
pub fn remembered_args(home: &Path, project: Option<&Path>, name: &str, args: Value) -> Value {
    let Some((_, keys)) = REMEMBERED.iter().find(|(n, _)| *n == name) else { return args };
    let project = project.map(|p| p.display().to_string().to_lowercase()).unwrap_or_default();
    let mut all: BTreeMap<String, BTreeMap<String, serde_json::Map<String, Value>>> =
        std::fs::read_to_string(prefs_path(home)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut args = match args {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    let stored = all.entry(project).or_default().entry(name.to_string()).or_default();
    let mut changed = false;
    for key in keys.iter() {
        match args.get(*key).cloned() {
            Some(Value::String(s)) if s == "default" => {
                args.remove(*key);
                changed |= stored.remove(*key).is_some();
            }
            Some(v) if !v.is_null() => {
                changed |= stored.insert(key.to_string(), v.clone()).as_ref() != Some(&v);
            }
            _ => {
                if let Some(v) = stored.get(*key) {
                    args.insert(key.to_string(), v.clone());
                }
            }
        }
    }
    if changed {
        let p = prefs_path(home);
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, serde_json::to_string_pretty(&all).unwrap_or_default());
    }
    Value::Object(args)
}

/// The approval line for a first run: what the script says it is and what it may do.
pub fn approval_summary(p: &Prepared) -> String {
    let phases = if p.meta.phases.is_empty() { String::new() } else { format!(" · phases: {}", p.meta.phases.join(" → ")) };
    format!(
        "run team task `{}` (first run of this script): {}{phases} · up to {} agents, {} at a time, in a crew sandbox",
        p.name,
        p.meta.description.trim(),
        crate::MAX_AGENTS,
        crate::default_concurrency(crate::MAX_CONCURRENCY)
    )
}

/// Roots for a session: its workspace, the profile home, plugin bundles.
pub fn roots(home: &Path, workspace: Option<&Path>, plugins: Vec<PathBuf>) -> Roots {
    Roots { workspaces: workspace.map(Path::to_path_buf).into_iter().collect(), home: Some(home.to_path_buf()), plugins }
}

/// The saved list for the skills index and `/workflows`.
pub fn saved_list(roots: &Roots) -> Vec<marlowe_proto::SavedWorkflow> {
    roots.list().into_iter().map(|(s, _)| s).collect()
}

/// Keeps `saved` usable from here without importing the module.
pub use saved::index as saved_index;
