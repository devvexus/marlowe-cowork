//! `items_from` of the `workflow` short form: the harness lists the real items in the workspace,
//! so a model never names paths or packages it has not seen.
//!
//! - `folders` (dirs, directories): top-level directories, minus VCS data, build output and what
//!   `.gitignore` names; with fewer than two, the top-level files are one more item.
//! - `packages` (crates, modules, projects): Cargo workspace members, else directories at depth 2
//!   or less holding a manifest (`Cargo.toml`, `package.json`, `pyproject.toml`, `setup.py`,
//!   `go.mod`), else the folders, with a note.
//! - `commits:N` (commits, last N commits): `git log`, default 5, at most 20.
//! - a glob (`files:<glob>`, anything with `*`): the files under the same skip rules.
//!
//! At most [`MAX_ITEMS`] items (a combining step has to fit them in a small window); more, or none,
//! is an error that says what is there.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ignore::WalkBuilder;

use crate::search::walk::{is_skipped, Filters};

/// The most items one short-form run takes.
pub const MAX_ITEMS: usize = 24;
const DEFAULT_COMMITS: usize = 5;
const MAX_COMMITS: usize = 20;
/// Manifests that make a directory a package.
const MANIFESTS: [&str; 5] = ["Cargo.toml", "package.json", "pyproject.toml", "setup.py", "go.mod"];
/// Files a glob walk looks at before it gives up counting.
const WALK_CAP: usize = 20_000;

/// The resolved items, and a line for the receipt when the harness chose something other than what
/// was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Items {
    pub items: Vec<String>,
    pub note: Option<String>,
}

fn items(items: Vec<String>) -> Items {
    Items { items, note: None }
}

/// Resolves `selector` against `workspace`.
pub fn resolve(selector: &str, workspace: &Path) -> Result<Items, String> {
    let raw = selector.trim();
    let low = raw.to_lowercase();
    let low = low.trim_matches(|c: char| c == '`' || c == '"' || c == '\'').trim().to_string();
    let found = match low.as_str() {
        "folders" | "folder" | "dirs" | "dir" | "directories" | "directory" | "subfolders" | "top-level folders" | "top level folders" | "top-level directories"
        | "top level directories" | "top-level" | "toplevel" => folders(workspace)?,
        "packages" | "package" | "crates" | "crate" | "modules" | "module" | "projects" | "project" | "workspace members" => packages(workspace)?,
        l if is_commits(l) => commits(l, workspace)?,
        _ if raw.starts_with("files:") || raw.contains(['*', '?', '{']) => glob(raw.strip_prefix("files:").unwrap_or(raw).trim(), workspace)?,
        _ => {
            return Err(format!(
                "`items_from` \"{raw}\" is not one of: folders | packages | commits:N | a glob such as src/**/*.rs. {}",
                there(workspace)
            ))
        }
    };
    if found.items.len() > MAX_ITEMS {
        return Err(format!(
            "`{raw}` gives {} items; at most {MAX_ITEMS} (each answer has to fit one combining step). Narrow it: a glob such as {}, or commits:N.",
            found.items.len(),
            found.items.first().map(|f| narrow_example(f)).unwrap_or_else(|| "src/*".into())
        ));
    }
    Ok(found)
}

fn narrow_example(first: &str) -> String {
    match first.split_once('/') {
        Some((dir, _)) => format!("{dir}/*"),
        None => "src/*".into(),
    }
}

fn is_commits(l: &str) -> bool {
    l.starts_with("commits") || l.starts_with("commit") || l.contains(" commit") || l.starts_with("git log") || l.starts_with("recent commit")
}

// ---------------------------------------------------------------- what is there

/// A line naming what the workspace holds at its top level (for errors and the reminder).
pub fn there(workspace: &Path) -> String {
    let (dirs, files) = top_level(workspace);
    let mut parts = Vec::new();
    if !dirs.is_empty() {
        parts.push(format!("folders: {}", list(&dirs, 12)));
    }
    if !files.is_empty() {
        parts.push(format!("files: {}", list(&files, 8)));
    }
    if parts.is_empty() {
        format!("{} is empty.", workspace.display())
    } else {
        format!("In {}: {}.", workspace.display(), parts.join("; "))
    }
}

fn list(names: &[String], max: usize) -> String {
    let mut s = names.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > max {
        s.push_str(&format!(", and {} more", names.len() - max));
    }
    s
}

/// The top-level directories and files, sorted, skipping VCS data, build output and ignored paths.
pub fn top_level(workspace: &Path) -> (Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in walker(workspace, 1).build().flatten() {
        if e.depth() != 1 {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        match e.file_type() {
            Some(t) if t.is_dir() => dirs.push(name),
            Some(t) if t.is_file() => files.push(name),
            _ => {}
        }
    }
    dirs.sort();
    files.sort();
    (dirs, files)
}

/// A walk of `workspace` to `depth` under rg's ignore rules and the shared skip list. It does not
/// need a `.git` directory to honour `.gitignore`.
fn walker(workspace: &Path, depth: usize) -> WalkBuilder {
    let mut b = WalkBuilder::new(workspace);
    b.hidden(false).follow_links(false).require_git(false).git_global(false).max_depth(Some(depth)).add_custom_ignore_filename(".rgignore");
    b.filter_entry(|e| e.depth() == 0 || !(e.file_type().is_some_and(|t| t.is_dir()) && is_skipped(&e.file_name().to_string_lossy())));
    b
}

// ---------------------------------------------------------------- folders

fn folders(workspace: &Path) -> Result<Items, String> {
    let (dirs, files) = top_level(workspace);
    let mut out = dirs;
    if out.len() < 2 && !files.is_empty() {
        out.push(format!("the top-level files ({})", list(&files, 10)));
    }
    if out.is_empty() {
        return Err(format!("There are no folders or files to read. {}", there(workspace)));
    }
    Ok(items(out))
}

// ---------------------------------------------------------------- packages

fn packages(workspace: &Path) -> Result<Items, String> {
    let members = cargo_members(workspace);
    if !members.is_empty() {
        return Ok(items(members));
    }
    let mut found: Vec<String> = Vec::new();
    for e in walker(workspace, 3).build().flatten() {
        if e.depth() == 0 || !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let name = e.file_name().to_string_lossy();
        if MANIFESTS.iter().any(|m| name == *m) {
            if let Some(dir) = e.path().parent().and_then(|d| d.strip_prefix(workspace).ok()) {
                let shown = slashed(dir);
                if !shown.is_empty() && !found.contains(&shown) {
                    found.push(shown);
                }
            }
        }
    }
    found.sort();
    if !found.is_empty() {
        return Ok(items(found));
    }
    let mut f = folders(workspace).map_err(|e| format!("No packages (no Cargo workspace or manifests within two folders). {e}"))?;
    f.note = Some("no packages found (no Cargo workspace or manifests within two folders), so these are the folders".into());
    Ok(f)
}

/// The existing member directories of a Cargo workspace at `workspace`'s root, `exclude` removed.
fn cargo_members(workspace: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(workspace.join("Cargo.toml")) else { return Vec::new() };
    let patterns = toml_list(&text, "members");
    let excluded = toml_list(&text, "exclude");
    let mut out: Vec<String> = Vec::new();
    for p in patterns {
        for dir in expand(workspace, p.trim_start_matches("./")) {
            let shown = slashed(dir.strip_prefix(workspace).unwrap_or(&dir));
            if dir.is_dir() && !excluded.iter().any(|x| x.trim_start_matches("./").trim_end_matches('/') == shown) && !out.contains(&shown) {
                out.push(shown);
            }
        }
    }
    out.sort();
    out
}

/// The strings of `key = [ ... ]` in a manifest's text (inside `[workspace]`, which is the only
/// table that has these keys), across lines, comments dropped.
fn toml_list(text: &str, key: &str) -> Vec<String> {
    let mut in_workspace = false;
    let mut collecting = false;
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if !collecting {
            if line.starts_with('[') {
                in_workspace = line == "[workspace]";
                continue;
            }
            let Some(rest) = line.strip_prefix(key).map(str::trim_start).and_then(|r| r.strip_prefix('=')) else { continue };
            if !in_workspace {
                continue;
            }
            collecting = true;
            push_strings(rest, &mut out);
            if rest.contains(']') {
                collecting = false;
            }
        } else {
            push_strings(line, &mut out);
            if line.contains(']') {
                collecting = false;
            }
        }
    }
    out
}

fn push_strings(s: &str, out: &mut Vec<String>) {
    let mut rest = s;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
}

/// A member pattern: path segments, each possibly with `*`.
fn expand(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut dirs = vec![base.to_path_buf()];
    for seg in pattern.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let mut next = Vec::new();
        for d in &dirs {
            if seg.contains('*') {
                if let Ok(rd) = std::fs::read_dir(d) {
                    for e in rd.flatten() {
                        if e.path().is_dir() && wild(seg, &e.file_name().to_string_lossy()) {
                            next.push(e.path());
                        }
                    }
                }
            } else {
                next.push(d.join(seg));
            }
        }
        dirs = next;
    }
    dirs
}

/// `*` matches any run of characters.
fn wild(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if parts.len() == 1 {
        return pattern == name;
    }
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut at = first.len();
    let end = name.len() - last.len();
    for mid in &parts[1..parts.len() - 1] {
        match name[at..end].find(mid) {
            Some(i) => at += i + mid.len(),
            None => return false,
        }
    }
    true
}

fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------- commits

fn commits(selector: &str, workspace: &Path) -> Result<Items, String> {
    let digits: String = selector.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
    let asked = digits.parse::<usize>().ok().filter(|n| *n > 0).unwrap_or(DEFAULT_COMMITS);
    let n = asked.min(MAX_COMMITS);
    let mut cmd = std::process::Command::new("git");
    cmd.args(["-c", "core.quotepath=false", "log", &format!("-{n}"), "--format=%h %s", "--no-color"]);
    let spec = marlowe_os::child::ChildSpec::new().with_creation_flags(marlowe_os::child::CREATE_NO_WINDOW).with_cwd(workspace);
    let limits = marlowe_os::child::RunLimits { timeout: Duration::from_secs(20), max_output_bytes: 64 * 1024 };
    let out = marlowe_os::child::run(cmd, &spec, limits, &|| false).map_err(|e| format!("could not run git: {e}. {}", there(workspace)))?;
    if out.timed_out || out.code() != 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().next().unwrap_or("it timed out").trim().to_string();
        return Err(format!("`git log` failed in {}: {why}. {}", workspace.display(), there(workspace)));
    }
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return Err(format!("{} has no commits yet. {}", workspace.display(), there(workspace)));
    }
    let note = (asked > MAX_COMMITS).then(|| format!("asked for {asked} commits; the most is {MAX_COMMITS}"));
    Ok(Items { items: lines, note })
}

// ---------------------------------------------------------------- glob

fn glob(spec: &str, workspace: &Path) -> Result<Items, String> {
    if spec.is_empty() {
        return Err(format!("`items_from` gave an empty glob. {}", there(workspace)));
    }
    let filters = Filters::default().with_globs(workspace, Some(workspace), spec)?;
    let mut b = walker(workspace, usize::MAX);
    if let Some(o) = filters.overrides {
        b.overrides(o);
    }
    let mut found: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for e in b.build().flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        seen += 1;
        if seen > WALK_CAP {
            break;
        }
        found.push(slashed(e.path().strip_prefix(workspace).unwrap_or(e.path())));
    }
    found.sort();
    if found.is_empty() {
        return Err(format!("No files match `{spec}`. {}", there(workspace)));
    }
    Ok(items(found))
}

#[cfg(test)]
mod tests;
