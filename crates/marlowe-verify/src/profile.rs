//! Project profiles: what a repository is built from and how to build, test, lint, serve and
//! migrate it, found from its manifests alone (no process is started).
//!
//! [`detect`] reads the root and its immediate subdirectories, so a repository with a `backend`
//! and a `frontend` yields both. `.marlowe/project.toml` overrides or extends the result
//! ([`crate::overrides`]). The profile also names the hub files for `build_v2` ([`crate::hubs`])
//! and the lockfile regeneration commands.
//!
//! | Stack | Found from |
//! |---|---|
//! | cargo | `Cargo.toml` |
//! | uv, poetry, pip | `uv.lock` / `[tool.uv]`, `poetry.lock` / `[tool.poetry]`, otherwise `pyproject.toml`, `requirements*.txt`, `setup.py` |
//! | npm, pnpm, yarn, bun | `package.json` plus the lockfile or `packageManager` |
//! | go | `go.mod` |
//! | cmake | `CMakeLists.txt` |
//! | make | `Makefile` |
//! | compose | `compose.y*ml`, `docker-compose.y*ml` |

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use marlowe_os::workspace::{WalkOpts, Workspace};
use serde::{Deserialize, Serialize};

use crate::cmd::{resolve_program, Cmd};
use crate::hubs::{self, HubClass, HubFile};

/// A technology the project is built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stack {
    Cargo,
    Uv,
    Poetry,
    Pip,
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Go,
    Cmake,
    Make,
    Compose,
}

impl Stack {
    pub const ALL: [Stack; 12] = [
        Stack::Cargo,
        Stack::Uv,
        Stack::Poetry,
        Stack::Pip,
        Stack::Npm,
        Stack::Pnpm,
        Stack::Yarn,
        Stack::Bun,
        Stack::Go,
        Stack::Cmake,
        Stack::Make,
        Stack::Compose,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stack::Cargo => "cargo",
            Stack::Uv => "uv",
            Stack::Poetry => "poetry",
            Stack::Pip => "pip",
            Stack::Npm => "npm",
            Stack::Pnpm => "pnpm",
            Stack::Yarn => "yarn",
            Stack::Bun => "bun",
            Stack::Go => "go",
            Stack::Cmake => "cmake",
            Stack::Make => "make",
            Stack::Compose => "compose",
        }
    }

    pub fn language(self) -> &'static str {
        match self {
            Stack::Cargo => "Rust",
            Stack::Uv | Stack::Poetry | Stack::Pip => "Python",
            Stack::Npm | Stack::Pnpm | Stack::Yarn | Stack::Bun => "JavaScript/TypeScript",
            Stack::Go => "Go",
            Stack::Cmake => "C/C++",
            Stack::Make => "make",
            Stack::Compose => "containers",
        }
    }

    /// Python and Node variants exclude each other within one directory and share a family when
    /// a subdirectory is compared against the root.
    fn family(self) -> u8 {
        match self {
            Stack::Uv | Stack::Poetry | Stack::Pip => 1,
            Stack::Npm | Stack::Pnpm | Stack::Yarn | Stack::Bun => 2,
            other => 10 + other as u8,
        }
    }
}

/// What a command is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Install,
    Format,
    Lint,
    Typecheck,
    Build,
    Test,
}

impl Kind {
    pub const ALL: [Kind; 6] = [Kind::Install, Kind::Format, Kind::Lint, Kind::Typecheck, Kind::Build, Kind::Test];
    /// The order `gate` runs: cheap checks first.
    pub const GATE: [Kind; 5] = [Kind::Format, Kind::Lint, Kind::Typecheck, Kind::Build, Kind::Test];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Install => "install",
            Kind::Format => "format",
            Kind::Lint => "lint",
            Kind::Typecheck => "typecheck",
            Kind::Build => "build",
            Kind::Test => "test",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        let s = s.trim().to_ascii_lowercase();
        Kind::ALL.into_iter().find(|k| k.name() == s).or(match s.as_str() {
            "check" | "types" | "tsc" => Some(Kind::Typecheck),
            "fmt" => Some(Kind::Format),
            "tests" => Some(Kind::Test),
            "deps" => Some(Kind::Install),
            _ => None,
        })
    }

    pub fn timeout(self) -> Duration {
        Duration::from_secs(match self {
            // Ten minutes, like a shell command; `timeout_ms` raises it (up to 30 minutes).
            Kind::Install | Kind::Build | Kind::Test | Kind::Format | Kind::Lint | Kind::Typecheck => 600,
        })
    }
}

/// Where a command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Detected,
    /// `.marlowe/project.toml`.
    Override,
}

/// A script a command runs on its own account: the body of a package.json script, which the
/// command line (`npm run test`) does not show. `name` is empty for the script the command names
/// and the lifecycle script's name for the ones the package manager runs around it (`pretest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub name: String,
    pub body: String,
}

/// One runnable command of the profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCmd {
    pub kind: Kind,
    pub cmd: Cmd,
    /// Working directory relative to the project root (`.` for the root).
    pub cwd: String,
    pub stack: Option<Stack>,
    pub source: Source,
    /// The registry host an install reaches (shown in the approval; recorded as approved). For a
    /// detected install it is read from the project's registry configuration, not assumed.
    pub registry_host: Option<String>,
    /// Further hosts the install reaches (a scoped registry, a git dependency, a mirror).
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// The check fails when it prints anything (`gofmt -l`).
    pub fail_on_output: bool,
    /// Run first, unless the path (relative to `cwd`) exists: `cmake -S . -B build`.
    pub pre: Option<(Cmd, String)>,
    /// Inserted before user-supplied extra arguments (`npm run test -- <args>`).
    pub args_separator: Option<String>,
    /// The program was found on `PATH` when the profile was built.
    pub available: bool,
    pub timeout_s: Option<u64>,
    /// What the command runs when a manifest says so: the package.json script `npm run test`
    /// names, the `pretest`/`posttest` the package manager runs around it, and the scripts those
    /// call. Shown in the approval, part of the fingerprint, and read by the sandbox policy,
    /// because the command line alone does not say what executes.
    #[serde(default)]
    pub runs: Vec<Script>,
}

impl ProjectCmd {
    pub fn new(kind: Kind, stack: Stack, cwd: &str, argv: &[&str]) -> Self {
        Self {
            kind,
            cmd: Cmd::new(argv),
            cwd: cwd.to_string(),
            stack: Some(stack),
            source: Source::Detected,
            registry_host: None,
            extra_hosts: Vec::new(),
            fail_on_output: false,
            pre: None,
            args_separator: None,
            available: true,
            timeout_s: None,
            runs: Vec::new(),
        }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout_s.map(Duration::from_secs).unwrap_or_else(|| self.kind.timeout())
    }

    pub fn registry(mut self, host: &str) -> Self {
        self.registry_host = Some(host.to_string());
        self
    }

    pub fn separator(mut self, sep: &str) -> Self {
        self.args_separator = Some(sep.to_string());
        self
    }
}

/// A dev server or other long-running process the project defines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDef {
    pub name: String,
    pub cmd: Cmd,
    pub cwd: String,
    pub ready_url: Option<String>,
    pub ready_port: Option<u16>,
    pub ready_log: Option<String>,
    pub env_allow: Vec<String>,
    pub source: Source,
    pub available: bool,
    /// See [`ProjectCmd::runs`].
    #[serde(default)]
    pub runs: Vec<Script>,
}

/// How the project migrates a database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Migrations {
    pub tool: String,
    pub up: Cmd,
    pub down: Option<Cmd>,
    pub cwd: String,
    /// Config files that may name a database (scanned for hard-coded real databases).
    pub config_files: Vec<String>,
    /// Environment set on the child; `{db_path}` and `{db_url}` are substituted.
    pub env: Vec<(String, String)>,
    pub source: Source,
    pub available: bool,
}

/// A lockfile and how to regenerate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub file: String,
    pub regen: Cmd,
    pub stack: Stack,
}

/// A stack found in a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackAt {
    pub stack: Stack,
    /// Relative to the project root (`.` for the root).
    pub dir: String,
}

/// Everything known about a project, from its files.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectProfile {
    pub root: PathBuf,
    pub stacks: Vec<StackAt>,
    pub commands: Vec<ProjectCmd>,
    pub services: Vec<ServiceDef>,
    pub migrations: Vec<Migrations>,
    pub lockfiles: Vec<Lockfile>,
    /// Hub files that exist now.
    pub hub_files: Vec<HubFile>,
    /// Hub patterns `.marlowe/project.toml` added.
    pub extra_hub_patterns: Vec<String>,
    /// Notes the model should see: a missing tool, an unreadable override.
    pub warnings: Vec<String>,
    /// `.marlowe/project.toml` was read.
    pub has_override: bool,
}

/// Directory names that are not sub-projects.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || hubs::SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(name)) || name.eq_ignore_ascii_case("docs")
}

/// Reads files through a [`Workspace`] so detection works the same in a container.
pub(crate) struct Fs<'a> {
    pub ws: &'a dyn Workspace,
}

impl Fs<'_> {
    pub fn is_file(&self, p: &Path) -> bool {
        self.ws.metadata(p).is_ok_and(|m| m.is_file)
    }

    pub fn is_dir(&self, p: &Path) -> bool {
        self.ws.metadata(p).is_ok_and(|m| m.is_dir)
    }

    /// The file as text, at most 1 MiB.
    pub fn text(&self, p: &Path) -> Option<String> {
        let bytes = self.ws.read(p).ok()?;
        let cut = bytes.len().min(1 << 20);
        Some(String::from_utf8_lossy(&bytes[..cut]).into_owned())
    }

    /// Names of files directly in `dir`.
    pub fn file_names(&self, dir: &Path) -> Vec<String> {
        let opts = WalkOpts { max_depth: Some(1), max_entries: Some(2000), skip_dirs: Vec::new() };
        self.ws
            .walk(dir, &opts)
            .map(|v| {
                v.into_iter()
                    .filter(|e| !e.is_dir)
                    .filter_map(|e| e.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Names of subdirectories directly in `dir` that may hold a sub-project, sorted.
    pub fn subdirs(&self, dir: &Path) -> Vec<String> {
        let opts = WalkOpts { max_depth: Some(1), max_entries: Some(2000), skip_dirs: Vec::new() };
        let mut v: Vec<String> = self
            .ws
            .walk(dir, &opts)
            .map(|v| {
                v.into_iter()
                    .filter(|e| e.is_dir)
                    .filter_map(|e| e.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .filter(|n| !skip_dir(n))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// What a directory scan collects.
#[derive(Default)]
pub(crate) struct Found {
    pub stacks: Vec<StackAt>,
    pub commands: Vec<ProjectCmd>,
    pub services: Vec<ServiceDef>,
    pub migrations: Vec<Migrations>,
    pub lockfiles: Vec<Lockfile>,
}

/// Builds the profile of the project at `root`, hub files included (walks the tree).
pub fn detect(ws: &dyn Workspace, root: &Path) -> ProjectProfile {
    let mut profile = detect_commands(ws, root);
    profile.hub_files = resolve_hubs(ws, &profile);
    profile
}

/// The profile without the hub-file walk: manifests only, a handful of reads. Every call plans
/// against this, fresh each time, so what is approved is what the files say at that moment.
pub fn detect_commands(ws: &dyn Workspace, root: &Path) -> ProjectProfile {
    let fs = Fs { ws };
    let mut found = Found::default();
    crate::stacks::scan_dir(&fs, root, ".", &mut found);
    let mut families: BTreeSet<u8> = found.stacks.iter().map(|s| s.stack.family()).collect();
    for sub in fs.subdirs(root).into_iter().take(16) {
        let mut inner = Found::default();
        crate::stacks::scan_dir(&fs, &root.join(&sub), &sub, &mut inner);
        // A stack the root already has is the root's own (a workspace member, a nested package).
        inner.stacks.retain(|s| !families.contains(&s.stack.family()));
        if inner.stacks.is_empty() {
            continue;
        }
        let keep: BTreeSet<Stack> = inner.stacks.iter().map(|s| s.stack).collect();
        families.extend(keep.iter().map(|s| s.family()));
        found.stacks.extend(inner.stacks);
        found.commands.extend(inner.commands.into_iter().filter(|c| c.stack.is_none_or(|s| keep.contains(&s))));
        found.services.extend(inner.services);
        found.migrations.extend(inner.migrations);
        found.lockfiles.extend(inner.lockfiles.into_iter().filter(|l| keep.contains(&l.stack)));
    }
    let mut profile = ProjectProfile {
        root: root.to_path_buf(),
        stacks: found.stacks,
        commands: found.commands,
        services: found.services,
        migrations: found.migrations,
        lockfiles: found.lockfiles,
        ..ProjectProfile::default()
    };
    crate::overrides::apply(&fs, root, &mut profile);
    mark_availability(&mut profile);
    profile
}

fn mark_availability(p: &mut ProjectProfile) {
    let root = p.root.clone();
    let avail = |cmd: &Cmd, cwd: &str| resolve_program(cmd.program(), Some(&root.join(cwd))).is_some();
    for c in &mut p.commands {
        c.available = avail(&c.cmd, &c.cwd);
    }
    for s in &mut p.services {
        s.available = avail(&s.cmd, &s.cwd);
    }
    for m in &mut p.migrations {
        m.available = avail(&m.up, &m.cwd);
    }
    let mut missing: Vec<String> = p
        .commands
        .iter()
        .filter(|c| !c.available)
        .map(|c| c.cmd.program_name())
        .chain(p.services.iter().filter(|s| !s.available).map(|s| s.cmd.program_name()))
        .collect();
    missing.sort();
    missing.dedup();
    for m in missing {
        p.warnings.push(format!("`{m}` is not on PATH; commands that use it fail until it is installed"));
    }
}

fn resolve_hubs(ws: &dyn Workspace, p: &ProjectProfile) -> Vec<HubFile> {
    let mut sets: Vec<(String, &[hubs::HubDef])> = Vec::new();
    for s in &p.stacks {
        let prefix = if s.dir == "." { String::new() } else { s.dir.clone() };
        let table = hubs::table(s.stack);
        if !sets.iter().any(|(pfx, t)| *pfx == prefix && std::ptr::eq(*t, table)) {
            sets.push((prefix, table));
        }
    }
    if !p.stacks.is_empty() || p.has_override {
        sets.push((String::new(), hubs::common()));
    }
    hubs::resolve(ws, &p.root, &sets, &p.extra_hub_patterns)
}

impl ProjectProfile {
    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty() && self.commands.is_empty() && self.services.is_empty()
    }

    /// The commands of `kind`, in the order they should run.
    pub fn commands_of(&self, kind: Kind) -> Vec<&ProjectCmd> {
        self.commands.iter().filter(|c| c.kind == kind).collect()
    }

    pub fn service(&self, name: &str) -> Option<&ServiceDef> {
        self.services.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// Hub file paths, for a planner that freezes them.
    pub fn hub_paths(&self) -> Vec<&str> {
        self.hub_files.iter().map(|h| h.path.as_str()).collect()
    }

    /// Lockfiles that exist, with their regeneration commands.
    pub fn lockfiles_present(&self) -> Vec<&Lockfile> {
        self.lockfiles
            .iter()
            .filter(|l| self.hub_files.iter().any(|h| h.class == HubClass::Lockfile && h.path.eq_ignore_ascii_case(&l.file)))
            .collect()
    }

    /// A readable account of the profile for the model.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "No project detected here (no manifest for cargo, python, node, go, cmake, make or compose). \
                    Add `.marlowe/project.toml` to describe the commands."
                .to_string();
        }
        let mut out = String::new();
        let stacks: Vec<String> = self
            .stacks
            .iter()
            .map(|s| if s.dir == "." { s.stack.name().to_string() } else { format!("{} ({})", s.stack.name(), s.dir) })
            .collect();
        out.push_str(&format!("stacks: {}\n", stacks.join(", ")));
        out.push_str("commands (project{op:\"run\", command:<name>}):\n");
        for kind in Kind::ALL {
            for c in self.commands_of(kind) {
                let mut line = format!("  {:<9} {}", kind.name(), c.cmd.display());
                if c.cwd != "." {
                    line.push_str(&format!("   (in {})", c.cwd));
                }
                if let Some((pre, skip)) = &c.pre {
                    line.push_str(&format!("   (first `{}` unless {skip} exists)", pre.display()));
                }
                if c.source == Source::Override {
                    line.push_str("   [override]");
                }
                if !c.available {
                    line.push_str("   [not on PATH]");
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
        if !self.services.is_empty() {
            out.push_str("services (service{op:\"start\", name:<name>}):\n");
            for s in &self.services {
                let ready = match (&s.ready_url, s.ready_port, &s.ready_log) {
                    (Some(u), _, _) => format!("ready at {u}"),
                    (None, Some(p), _) => format!("ready on port {p}"),
                    (None, None, Some(l)) => format!("ready when the log matches /{l}/"),
                    _ => "no readiness check".to_string(),
                };
                out.push_str(&format!(
                    "  {:<9} {}{}   {ready}{}\n",
                    s.name,
                    s.cmd.display(),
                    if s.cwd != "." { format!("   (in {})", s.cwd) } else { String::new() },
                    if s.source == Source::Override { "   [override]" } else { "" }
                ));
            }
        }
        for m in &self.migrations {
            out.push_str(&format!(
                "migrations: {} (project{{op:\"migrate\"}} runs `{}` against a throwaway sqlite database{})\n",
                m.tool,
                m.up.display(),
                if m.down.is_some() { ", and can round-trip down and up" } else { "" }
            ));
        }
        let locks = self.lockfiles_present();
        if !locks.is_empty() {
            out.push_str("lockfiles (regenerate, never merge by hand):\n");
            for l in locks {
                out.push_str(&format!("  {}   `{}`\n", l.file, l.regen.display()));
            }
        }
        if !self.hub_files.is_empty() {
            out.push_str(&format!("hub files ({}; parallel work must not both edit these):\n", self.hub_files.len()));
            for h in self.hub_files.iter().take(40) {
                out.push_str(&format!("  {:<40} {}: {}\n", h.path, h.class.name(), h.why));
            }
            if self.hub_files.len() > 40 {
                out.push_str(&format!("  ... and {} more\n", self.hub_files.len() - 40));
            }
        }
        for w in &self.warnings {
            out.push_str(&format!("note: {w}\n"));
        }
        out
    }
}

/// Whether `root` or one of its immediate subdirectories has a project manifest. Cheap (a
/// handful of `stat`s, no process); decides whether a local model is offered the verify tools.
pub fn looks_like_project(root: &Path) -> bool {
    const MANIFESTS: &[&str] = &[
        "Cargo.toml", "pyproject.toml", "setup.py", "requirements.txt", "package.json", "go.mod", "CMakeLists.txt",
        "Makefile", "GNUmakefile", "compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml",
        ".marlowe/project.toml",
    ];
    let has = |d: &Path| MANIFESTS.iter().any(|m| d.join(m).is_file());
    if has(root) {
        return true;
    }
    let Ok(rd) = std::fs::read_dir(root) else { return false };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !skip_dir(&e.file_name().to_string_lossy()))
        .take(40)
        .any(|e| has(&e.path()))
}
