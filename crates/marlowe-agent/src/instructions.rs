//! Instruction files (design 2.4 C1-C4, C12): `MARLOWE.md`, `AGENTS.md` and `CLAUDE.md` (plus
//! `.local.md` variants, `.claude/CLAUDE.md` and `.marlowe/MARLOWE.md`) at user level and from the
//! filesystem root down to the working directory; subdirectories' files lazily, when a file under
//! them is first touched; `@path` imports (depth 5, cycle-safe); and path-scoped rules
//! (`.marlowe/rules/*.md`, `.claude/rules/*.md` with `paths:` globs) loaded on the first matching
//! touch. Eager files go in the stable prefix ([`crate::system::PromptSections`]); lazy ones arrive
//! as a reminder appended after the step that touched them, so history stays append-only.
//!
//! [`InstructionsMiddleware`] owns the session's prompt sections: instructions, the skills index
//! and the output style ([`crate::commands`]), built at the first turn and on [`refresh`]
//! (`/memory`, `/add-dir`).
//!
//! Imports are confined: a user-level file (profile home, `~/.claude`, `~/.marlowe`) imports
//! anything, but a project, local, directory or added-root file only imports from the workspace
//! roots, the profile home and the user's `~/.claude` and `~/.marlowe`. Anything else is skipped
//! with a warning until the user trusts it once ([`trust`]): a cloned repository must not pull
//! `~/.ssh/config` into the prompt.

pub mod glob;
pub mod trust;
pub mod yaml;

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use marlowe_model::Message;
use serde_json::Value;

use crate::convo::Conversation;
use crate::middleware::{StepInfo, ToolMiddleware, TurnFlow};
use crate::system::PromptSections;
use crate::tools::{CallCtx, ToolCallReq, ToolOutcome};

/// Instruction file names, in load order within one directory.
pub const NAMES: [&str; 3] = ["MARLOWE.md", "AGENTS.md", "CLAUDE.md"];
/// Personal, not checked in; loaded after the shared files of the same directory.
pub const LOCAL_NAMES: [&str; 3] = ["MARLOWE.local.md", "AGENTS.local.md", "CLAUDE.local.md"];
/// `@path` imports nest at most this deep.
pub const MAX_IMPORT_DEPTH: usize = 5;
/// A larger instruction file is refused with a warning (it is not instructions).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Where an instruction file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    User,
    Project,
    Local,
    /// A subdirectory's file, loaded when a file under it was touched.
    Subdir,
    /// Imported with `@path` by another file.
    Import,
    /// A rule from `.marlowe/rules` / `.claude/rules` (always-on, or fired by a matching touch).
    Rule,
    /// A root added with `/add-dir`.
    AddedDir,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::User => "user",
            Origin::Project => "project",
            Origin::Local => "local",
            Origin::Subdir => "directory",
            Origin::Import => "import",
            Origin::Rule => "rule",
            Origin::AddedDir => "added-dir",
        }
    }
}

/// One loaded file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub origin: Origin,
    pub text: String,
}

/// A path-scoped rule: its body applies once a file matching one of `globs` (relative to `base`)
/// is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    pub path: PathBuf,
    pub base: PathBuf,
    pub globs: Vec<String>,
    pub body: String,
}

/// Where a session's instructions, commands, skills, agents, styles and plugins come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// The user's home (`~`), for `~/.claude/...`.
    pub user_home: Option<PathBuf>,
    /// The Marlowe profile home (`%LOCALAPPDATA%\marlowe\<profile>`).
    pub home: Option<PathBuf>,
    /// Workspace roots: the session's workspace first, then `/add-dir` roots.
    pub roots: Vec<PathBuf>,
    /// The working directory (defaults to the first root).
    pub cwd: Option<PathBuf>,
    /// Ancestor walk stops here (inclusive); `None` walks to the filesystem root.
    pub stop_at: Option<PathBuf>,
    /// Extra local plugin bundles (`--plugin-dir`, settings).
    pub plugin_roots: Vec<PathBuf>,
    /// Import targets outside the trusted areas that the user allowed (see the module docs).
    pub approved_imports: Vec<PathBuf>,
}

impl Sources {
    /// A session's sources: the user's home from the environment, the profile home and the
    /// workspace.
    pub fn for_session(home: Option<&Path>, workspace: Option<&Path>) -> Self {
        let user_home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        Self {
            user_home,
            home: home.map(Path::to_path_buf),
            roots: workspace.map(|w| vec![w.to_path_buf()]).unwrap_or_default(),
            cwd: workspace.map(Path::to_path_buf),
            stop_at: None,
            plugin_roots: Vec::new(),
            approved_imports: Vec::new(),
        }
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref().or(self.roots.first().map(PathBuf::as_path))
    }
}

/// The session's instruction set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// In the prefix, in load order (lowest precedence first).
    pub files: Vec<InstructionFile>,
    /// Path-scoped rules (fired ones are also in `files`).
    pub rules: Vec<PathRule>,
    pub fired_rules: BTreeSet<PathBuf>,
    /// Directories whose own files were already looked for.
    pub seen_dirs: BTreeSet<PathBuf>,
    /// Every file included so far (cycle and duplicate guard).
    pub seen_files: BTreeSet<String>,
    pub roots: Vec<PathBuf>,
    pub user_home: Option<PathBuf>,
    /// Where a project file's imports may point without asking, and the targets allowed beyond.
    pub trusted_areas: Vec<PathBuf>,
    pub approved_imports: Vec<PathBuf>,
    /// Imports skipped because they point outside the trusted areas (asked about by [`refresh`]).
    pub blocked_imports: Vec<BlockedImport>,
    /// Refusals and oddities, shown with the prefix report (never silent).
    pub warnings: Vec<String>,
}

/// An `@` import a project file made of a file outside the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedImport {
    pub importer: PathBuf,
    pub raw: String,
    pub target: PathBuf,
}

fn key(p: &Path) -> String {
    let s = normalize(p).to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Lexically normalised (`.` and `..` folded), without touching the disk.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` lies inside `root` (lexically, case-insensitive on Windows).
pub fn inside(root: &Path, path: &Path) -> bool {
    let (r, p) = (key(root), key(path));
    p == r || p.starts_with(&format!("{}/", r.trim_end_matches('/')))
}

pub(crate) fn read_file(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_FILE_BYTES {
        warnings.push(format!("{} is {} bytes; files over {MAX_FILE_BYTES} bytes are not loaded", path.display(), meta.len()));
        return None;
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).trim_start_matches('\u{feff}').to_string()),
        Err(e) => {
            warnings.push(format!("{} could not be read: {e}", path.display()));
            None
        }
    }
}

impl Instructions {
    /// Loads user-level files, the ancestors of the working directory (top down), each root's
    /// always-on rules, and added roots' own files. Path-scoped rules wait for a matching touch.
    pub fn load(src: &Sources) -> Self {
        let mut me = Instructions { roots: src.roots.clone(), user_home: src.user_home.clone(), ..Default::default() };
        me.trusted_areas = src.roots.iter().map(|r| normalize(r)).collect();
        me.trusted_areas.extend(src.home.iter().map(|h| normalize(h)));
        if let Some(u) = &src.user_home {
            me.trusted_areas.push(normalize(&u.join(".claude")));
            me.trusted_areas.push(normalize(&u.join(".marlowe")));
        }
        me.approved_imports = src.approved_imports.iter().map(|p| normalize(p)).collect();
        if let Some(h) = &src.home {
            for n in ["MARLOWE.md", "AGENTS.md"] {
                me.add_file(&h.join(n), Origin::User, 0);
            }
            me.load_rules_in(&h.join("rules"), h, true);
        }
        if let Some(u) = &src.user_home {
            me.add_file(&u.join(".claude").join("CLAUDE.md"), Origin::User, 0);
            me.add_file(&u.join(".marlowe").join("MARLOWE.md"), Origin::User, 0);
            me.load_rules_in(&u.join(".claude").join("rules"), u, true);
        }
        if let Some(cwd) = src.cwd() {
            let cwd = normalize(cwd);
            let mut chain: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
            if let Some(stop) = &src.stop_at {
                if let Some(i) = chain.iter().position(|a| key(a) == key(stop)) {
                    chain.truncate(i + 1);
                }
            }
            chain.reverse();
            for dir in chain {
                me.load_dir(&dir, false);
            }
        }
        for root in &src.roots {
            let root = normalize(root);
            if !src.cwd().is_some_and(|c| inside(&root, c)) {
                me.load_dir(&root, true);
            }
            me.load_project_rules(&root);
        }
        me
    }

    /// The directory's own files (shared, then personal). `added` marks an `/add-dir` root.
    fn load_dir(&mut self, dir: &Path, added: bool) {
        if !self.seen_dirs.insert(normalize(dir)) {
            return;
        }
        let shared = if added { Origin::AddedDir } else { Origin::Project };
        for n in NAMES {
            self.add_file(&dir.join(n), shared, 0);
        }
        self.add_file(&dir.join(".claude").join("CLAUDE.md"), shared, 0);
        self.add_file(&dir.join(".marlowe").join("MARLOWE.md"), shared, 0);
        for n in LOCAL_NAMES {
            self.add_file(&dir.join(n), Origin::Local, 0);
        }
    }

    fn load_project_rules(&mut self, root: &Path) {
        self.load_rules_in(&root.join(".marlowe").join("rules"), root, false);
        self.load_rules_in(&root.join(".claude").join("rules"), root, false);
    }

    /// Every `*.md` under `dir` (recursively): rules with `paths:` wait, others load now. `user`:
    /// the user's own rules, whose imports are not confined.
    fn load_rules_in(&mut self, dir: &Path, base: &Path, user: bool) {
        let mut files = Vec::new();
        md_files(dir, &mut files);
        for f in files {
            let Some(text) = read_file(&f, &mut self.warnings) else { continue };
            let (front, body) = match yaml::parse_document(&text) {
                Ok(v) => v,
                Err(e) => {
                    self.warnings.push(format!("{}: frontmatter not understood ({e}); loaded as plain text", f.display()));
                    (yaml::Yaml::Null, text.as_str())
                }
            };
            let globs = front.get_any(&["paths", "globs"]).map(yaml::Yaml::list).unwrap_or_default();
            if globs.is_empty() {
                if self.seen_files.insert(key(&f)) {
                    let body = body.trim().to_string();
                    self.files.push(InstructionFile { path: f.clone(), origin: Origin::Rule, text: body.clone() });
                    let dir = f.parent().unwrap_or(base).to_path_buf();
                    self.expand_imports(&f, &dir, &body, 1, user);
                }
            } else if !self.rules.iter().any(|r| r.path == f) {
                self.rules.push(PathRule { path: f, base: base.to_path_buf(), globs, body: body.trim().to_string() });
            }
        }
    }

    /// Adds one file (and its imports) if it exists and was not seen.
    fn add_file(&mut self, path: &Path, origin: Origin, depth: usize) -> bool {
        self.add_file_as(path, origin, depth, origin == Origin::User)
    }

    /// [`Self::add_file`]; `user`: the file is the user's own, so its imports are not confined.
    fn add_file_as(&mut self, path: &Path, origin: Origin, depth: usize, user: bool) -> bool {
        if !path.is_file() || !self.seen_files.insert(key(path)) {
            return false;
        }
        let Some(text) = read_file(path, &mut self.warnings) else { return false };
        self.files.push(InstructionFile { path: path.to_path_buf(), origin, text: text.trim().to_string() });
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        self.expand_imports(path, &dir, &text, depth + 1, user);
        true
    }

    /// Whether a project file may import `target` without asking.
    fn import_allowed(&self, target: &Path) -> bool {
        self.trusted_areas.iter().any(|a| inside(a, target)) || self.approved_imports.iter().any(|a| key(a) == key(target))
    }

    fn expand_imports(&mut self, importer: &Path, dir: &Path, text: &str, depth: usize, user: bool) {
        for raw in find_imports(text) {
            let target = resolve_import(&raw, dir, self.user_home.as_deref());
            if !target.is_file() {
                continue;
            }
            if depth > MAX_IMPORT_DEPTH {
                self.warnings.push(format!("@{raw}: imports nest deeper than {MAX_IMPORT_DEPTH}; not loaded"));
                continue;
            }
            if !user && !self.import_allowed(&target) {
                if !self.blocked_imports.iter().any(|b| key(&b.target) == key(&target)) {
                    self.warnings.push(format!(
                        "@{raw} in {} points outside the workspace ({}); not loaded until you allow it",
                        importer.display(),
                        target.display()
                    ));
                    self.blocked_imports.push(BlockedImport { importer: importer.to_path_buf(), raw, target });
                }
                continue;
            }
            self.add_file_as(&target, Origin::Import, depth, user);
        }
    }

    /// Files that become relevant because `path` was touched: subdirectory instruction files
    /// between its root and its directory, and path rules it matches. Each loads once.
    pub fn touch(&mut self, path: &Path) -> Vec<InstructionFile> {
        let path = normalize(path);
        let before = self.files.len();
        let roots: Vec<PathBuf> = self.roots.iter().map(|r| normalize(r)).collect();
        for root in roots {
            if !inside(&root, &path) {
                continue;
            }
            let mut dirs: Vec<PathBuf> = Vec::new();
            let mut cur = if path.is_dir() { Some(path.clone()) } else { path.parent().map(Path::to_path_buf) };
            while let Some(d) = cur {
                if key(&d) == key(&root) || !inside(&root, &d) {
                    break;
                }
                dirs.push(d.clone());
                cur = d.parent().map(Path::to_path_buf);
            }
            for d in dirs.into_iter().rev() {
                if self.seen_dirs.insert(d.clone()) {
                    for n in NAMES.iter().chain(LOCAL_NAMES.iter()) {
                        self.add_file(&d.join(n), Origin::Subdir, 0);
                    }
                }
            }
        }
        let mut fired = Vec::new();
        for r in &self.rules {
            let base = normalize(&r.base);
            if self.fired_rules.contains(&r.path) || !inside(&base, &path) {
                continue;
            }
            let rel = path.strip_prefix(&base).map(Path::to_path_buf).unwrap_or_else(|_| path.clone());
            let rel = rel.to_string_lossy().replace('\\', "/");
            if r.globs.iter().any(|g| glob::glob_match(g, &rel)) {
                fired.push(r.clone());
            }
        }
        let mut out: Vec<InstructionFile> = self.files[before..].to_vec();
        for r in fired {
            self.fired_rules.insert(r.path.clone());
            self.seen_files.insert(key(&r.path));
            let f = InstructionFile { path: r.path.clone(), origin: Origin::Rule, text: r.body.clone() };
            self.files.push(f.clone());
            out.push(f);
        }
        out
    }

    /// The full instruction text for the prefix (no cap).
    pub fn render(&self) -> String {
        render_files(&self.files)
    }
}

/// The `<instructions>` block for `files` (empty for none).
pub fn render_files(files: &[InstructionFile]) -> String {
    let files: Vec<&InstructionFile> = files.iter().filter(|f| !f.text.trim().is_empty()).collect();
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<instructions>\nThe user's and the project's instruction files (MARLOWE.md, AGENTS.md, CLAUDE.md, their imports \
         and rules). They override default behaviour: follow them exactly as written.\n",
    );
    for f in files {
        out.push_str(&format!("\n<file path=\"{}\" kind=\"{}\">\n{}\n</file>\n", f.path.display(), f.origin.as_str(), f.text.trim()));
    }
    out.push_str("</instructions>");
    out
}

/// A reminder carrying files loaded mid-session (a touched subdirectory, a fired rule).
pub fn render_reminder(files: &[InstructionFile]) -> Option<String> {
    let body: Vec<String> = files
        .iter()
        .filter(|f| !f.text.trim().is_empty())
        .map(|f| format!("<file path=\"{}\" kind=\"{}\">\n{}\n</file>", f.path.display(), f.origin.as_str(), f.text.trim()))
        .collect();
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "<system-reminder>\nInstructions that apply to the files you just touched; follow them:\n{}\n</system-reminder>",
        body.join("\n")
    ))
}

pub(crate) fn md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            md_files(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")) {
            out.push(p);
        }
    }
}

/// `@path` tokens outside code fences and inline code: at the start of a line or after
/// whitespace or `(`; trailing punctuation is not part of the path.
pub fn find_imports(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            continue;
        }
        let mut in_code = false;
        let mut prev = ' ';
        for (pos, c) in line.char_indices() {
            if c == '`' {
                in_code = !in_code;
            } else if c == '@' && !in_code && (prev.is_whitespace() || prev == '(') {
                let rest = &line[pos + 1..];
                let end = rest.find(|ch: char| ch.is_whitespace() || ch == '`' || ch == ')').unwrap_or(rest.len());
                let tok = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '\'']);
                if !tok.is_empty() && !tok.contains('@') {
                    out.push(tok.to_string());
                }
            }
            prev = c;
        }
    }
    out
}

fn resolve_import(raw: &str, dir: &Path, user_home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(h)) = (raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")), user_home) {
        return normalize(&h.join(rest));
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&dir.join(p))
    }
}

/// Workspace roots beyond the session's own (`/add-dir`, `--add-dir`), kept in
/// `Conversation::ext`. Their instruction files load, and the loop runs a call whose paths are
/// all under an added root on tools rooted there ([`crate::worktree::step_host`]), so its writes
/// are workspace writes and its reads do not taint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceRoots {
    pub extra: Vec<PathBuf>,
}

impl WorkspaceRoots {
    /// Adds `dir`; false when it was already a root.
    pub fn add(&mut self, dir: &Path) -> bool {
        let dir = normalize(dir);
        if self.extra.iter().any(|d| key(d) == key(&dir)) {
            return false;
        }
        self.extra.push(dir);
        true
    }

    /// Whether `path` is inside the session workspace or an added root.
    pub fn contains(&self, workspace: Option<&Path>, path: &Path) -> bool {
        workspace.is_some_and(|w| inside(w, path)) || self.extra.iter().any(|r| inside(r, path))
    }
}

/// The session's instruction state in `Conversation::ext`.
#[derive(Debug, Clone, Default)]
pub struct InstructionState {
    pub sources: Sources,
    pub loaded: Instructions,
}

/// Paths touched by tool calls this step (recorded by `after_call` on tool threads, drained by
/// `after_step`).
#[derive(Debug, Clone, Default)]
pub struct Touched(pub Vec<PathBuf>);

/// Argument names that carry a path a call reads or writes.
const PATH_KEYS: [&str; 5] = ["path", "file_path", "notebook_path", "cwd", "dir"];

/// The path strings in a call's arguments, as the model gave them.
pub fn path_values(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = PATH_KEYS.iter().filter_map(|k| args.get(*k).and_then(Value::as_str)).map(str::to_string).collect();
    if let Some(edits) = args.get("edits").and_then(Value::as_array) {
        for e in edits {
            if let Some(s) = e.get("path").or_else(|| e.get("file_path")).and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// The paths in a call's arguments (resolved against the workspace).
pub fn call_paths(args: &Value, workspace: Option<&Path>) -> Vec<PathBuf> {
    path_values(args)
        .iter()
        .map(|s| {
            let p = Path::new(s);
            let abs = if p.is_absolute() { p.to_path_buf() } else { workspace.map(|w| w.join(p)).unwrap_or_else(|| p.to_path_buf()) };
            normalize(&abs)
        })
        .collect()
}

/// Rebuilds the session's prompt sections from its sources (first turn, `/memory`, `/add-dir`).
/// Lazy files already loaded stay loaded. Returns the new sections.
pub fn refresh(convo: &mut Conversation) -> PromptSections {
    let mut state = convo.ext.get::<InstructionState>().cloned().unwrap_or_default();
    let extra = convo.ext.get::<WorkspaceRoots>().map(|r| r.extra.clone()).unwrap_or_default();
    for r in extra {
        if !state.sources.roots.iter().any(|x| key(x) == key(&r)) {
            state.sources.roots.push(r);
        }
    }
    let touched: Vec<PathBuf> =
        state.loaded.files.iter().filter(|f| f.origin == Origin::Subdir).map(|f| f.path.clone()).collect();
    let fired = state.loaded.fired_rules.clone();
    let mut loaded = Instructions::load(&state.sources);
    if let Some(approved) = ask_imports(convo, &state.sources, &loaded.blocked_imports) {
        state.sources.approved_imports.extend(approved);
        loaded = Instructions::load(&state.sources);
    }
    for p in touched {
        loaded.touch(&p);
    }
    for r in fired {
        if let Some(rule) = loaded.rules.iter().find(|x| x.path == r).cloned() {
            if loaded.fired_rules.insert(rule.path.clone()) {
                loaded.files.push(InstructionFile { path: rule.path, origin: Origin::Rule, text: rule.body });
            }
        }
    }
    let catalog = crate::commands::Catalog::load(&state.sources);
    let previous = convo.ext.get::<PromptSections>().cloned().unwrap_or_default();
    let mut sections = PromptSections {
        instructions: loaded.render(),
        skills_index: catalog.skills_index(),
        output_style: crate::commands::active_output_style(&state.sources, &catalog).map(|s| s.render()).unwrap_or_default(),
        ..previous
    };
    sections.warnings = loaded.warnings.iter().chain(catalog.warnings.iter()).cloned().collect();
    state.loaded = loaded;
    convo.ext.insert(state);
    convo.ext.insert(sections.clone());
    sections
}

/// Import digests the user declined this session (not asked again until the next one).
#[derive(Debug, Clone, Default)]
struct DeclinedImports(BTreeSet<String>);

/// Asks the session's [`trust::Trust`] once about `blocked` imports. `Some(targets)` when the
/// user allowed them (now or before, by digest of the project and each file).
fn ask_imports(convo: &mut Conversation, sources: &Sources, blocked: &[BlockedImport]) -> Option<Vec<PathBuf>> {
    if blocked.is_empty() {
        return None;
    }
    let svc = convo.ext.get::<trust::TrustService>()?.0.clone();
    let project = sources.roots.first().map(|r| key(r)).unwrap_or_default();
    let digests: Vec<String> =
        blocked.iter().filter_map(|b| trust::file_digest(&format!("import into {project}"), &b.target)).collect();
    if digests.len() != blocked.len() {
        return None;
    }
    let all = digests.join(",");
    if convo.ext.get::<DeclinedImports>().is_some_and(|d| d.0.contains(&all)) {
        return None;
    }
    let lines: Vec<String> =
        blocked.iter().map(|b| format!("{} imports {} (outside the workspace)", b.importer.display(), b.target.display())).collect();
    let subject =
        format!("instruction imports from outside {}", sources.roots.first().map(|r| r.display().to_string()).unwrap_or_default());
    if svc.trust(&convo.session, &subject, &lines, &digests) {
        Some(blocked.iter().map(|b| b.target.clone()).collect())
    } else {
        convo.ext.get_or_default::<DeclinedImports>().0.insert(all);
        None
    }
}

/// Loads the session's instructions, skills index and output style at its first turn, and
/// delivers lazily loaded files after the step that touched them.
pub struct InstructionsMiddleware {
    sources: Sources,
    trust: Option<Arc<dyn trust::Trust>>,
}

impl InstructionsMiddleware {
    pub fn new(sources: Sources) -> Arc<Self> {
        Arc::new(Self { sources, trust: None })
    }

    /// With `trust` asking about imports from outside the workspace (the daemon's).
    pub fn with_trust(sources: Sources, trust: Arc<dyn trust::Trust>) -> Arc<Self> {
        Arc::new(Self { sources, trust: Some(trust) })
    }
}

impl ToolMiddleware for InstructionsMiddleware {
    fn name(&self) -> &str {
        "instructions"
    }

    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        if convo.ext.get::<InstructionState>().is_none() {
            let mut sources = self.sources.clone();
            if sources.roots.is_empty() {
                if let Some(w) = &convo.workspace {
                    sources.roots.push(w.clone());
                    sources.cwd.get_or_insert_with(|| w.clone());
                }
            }
            convo.ext.insert(InstructionState { sources, loaded: Instructions::default() });
            if let Some(t) = &self.trust {
                convo.ext.insert(trust::TrustService(t.clone()));
            }
            refresh(convo);
        }
        TurnFlow::Continue
    }

    fn after_call(&self, call: &ToolCallReq, cx: &CallCtx, _outcome: &mut ToolOutcome) {
        let paths = call_paths(&call.args, cx.workspace.as_deref());
        if !paths.is_empty() {
            cx.scope.with_ext(|e| e.get_or_default::<Touched>().0.extend(paths));
        }
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        let touched = std::mem::take(&mut convo.ext.get_or_default::<Touched>().0);
        if touched.is_empty() {
            return Vec::new();
        }
        let Some(state) = convo.ext.get_mut::<InstructionState>() else { return Vec::new() };
        let mut fresh = Vec::new();
        for p in touched {
            fresh.extend(state.loaded.touch(&p));
        }
        render_reminder(&fresh).map(Message::user).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_skip_code_and_emails() {
        let t = "See @docs/a.md and (@./b.md).\n`@not/this.md` mail me@x.com\n```\n@fenced.md\n```\n@~/c.md";
        assert_eq!(find_imports(t), ["docs/a.md", "./b.md", "~/c.md"]);
    }

    #[test]
    fn inside_is_lexical_and_case_insensitive_on_windows() {
        assert!(inside(Path::new("C:/a/b"), Path::new("C:/a/b/c/../d.rs")));
        assert!(!inside(Path::new("C:/a/b"), Path::new("C:/a/bc/d.rs")));
        if cfg!(windows) {
            assert!(inside(Path::new("C:/A/B"), Path::new("c:\\a\\b\\x")));
        }
    }
}
