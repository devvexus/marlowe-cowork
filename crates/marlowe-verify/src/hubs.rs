//! Hub files: the files parallel tasks collide on (manifests, lockfiles, module and route
//! registries, schema slots, CI). `build_v2` freezes them after its contract stage, hands
//! changes to them to the lead, and never lets an agent merge a lockfile by hand.
//!
//! Each stack has a table of anchored glob patterns relative to the directory the stack lives in.
//! `*` stays inside a path segment, `**` crosses segments, `{a,b}` alternates. Matching is
//! ASCII case-insensitive (Windows file names).

use std::path::Path;

use marlowe_os::workspace::{WalkOpts, Workspace};
use serde::{Deserialize, Serialize};

use crate::profile::Stack;

/// What kind of collision a hub file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubClass {
    /// Declares packages, members or dependencies.
    Manifest,
    /// Generated from manifests; regenerated, never merged by hand.
    Lockfile,
    /// Lists modules, routes, plugins or settings that many tasks add to.
    Registry,
    /// Tool configuration (compiler, linter, test runner, CI).
    Config,
    /// Database schema or migration slots.
    Schema,
}

impl HubClass {
    pub fn name(self) -> &'static str {
        match self {
            HubClass::Manifest => "manifest",
            HubClass::Lockfile => "lockfile",
            HubClass::Registry => "registry",
            HubClass::Config => "config",
            HubClass::Schema => "schema",
        }
    }
}

/// One pattern in a stack's table.
#[derive(Debug, Clone, Copy)]
pub struct HubDef {
    pub pattern: &'static str,
    pub why: &'static str,
    pub class: HubClass,
}

const fn hub(pattern: &'static str, class: HubClass, why: &'static str) -> HubDef {
    HubDef { pattern, why, class }
}

use HubClass::{Config, Lockfile, Manifest, Registry, Schema};

const CARGO: &[HubDef] = &[
    hub("Cargo.toml", Manifest, "workspace members and shared dependency versions"),
    hub("**/Cargo.toml", Manifest, "crate dependencies and features"),
    hub("Cargo.lock", Lockfile, "regenerate with cargo, never merge by hand"),
    hub("{rust-toolchain,rust-toolchain.toml}", Config, "pinned toolchain"),
    hub(".cargo/config.toml", Config, "build flags and aliases"),
    hub("{,*/,crates/*/,packages/*/}build.rs", Config, "build scripts"),
    hub("{deny,clippy,rustfmt,.rustfmt}.toml", Config, "lint and format policy"),
    hub("**/src/lib.rs", Registry, "module declarations and public exports"),
    hub("**/src/main.rs", Registry, "module declarations and wiring"),
    hub("**/mod.rs", Registry, "child module declarations"),
];

const PYTHON: &[HubDef] = &[
    hub("pyproject.toml", Manifest, "dependencies and tool configuration"),
    hub("{requirements*.txt,setup.py,setup.cfg,Pipfile}", Manifest, "dependency lists"),
    hub("{uv.lock,poetry.lock,Pipfile.lock}", Lockfile, "regenerate with the package manager"),
    hub("**/__init__.py", Registry, "package exports"),
    hub("**/conftest.py", Config, "shared test fixtures"),
    hub("**/settings.py", Registry, "installed apps and middleware"),
    hub("**/urls.py", Registry, "route table"),
    hub("manage.py", Config, "framework entry point"),
    hub("{alembic.ini,**/alembic/env.py}", Schema, "migration environment"),
    hub("**/migrations/versions/*.py", Schema, "migration chain: one head, ordered"),
    hub("**/migrations/0*.py", Schema, "migration chain: numbered slots"),
    hub("{tox.ini,pytest.ini,mypy.ini,ruff.toml,.ruff.toml,pyrightconfig.json}", Config, "test and lint policy"),
    hub(".python-version", Config, "interpreter version"),
];

const NODE: &[HubDef] = &[
    hub("package.json", Manifest, "dependencies and scripts"),
    hub("{pnpm-workspace.yaml,lerna.json,turbo.json,nx.json}", Manifest, "workspace layout"),
    hub("{package-lock.json,pnpm-lock.yaml,yarn.lock,bun.lock,bun.lockb,npm-shrinkwrap.json}", Lockfile, "regenerate with the package manager"),
    hub("{tsconfig,jsconfig}{,.*}.json", Config, "compiler options and path aliases"),
    hub("{vite,next,nuxt,astro,svelte,webpack,rollup,esbuild,vitest,jest,playwright,tailwind,postcss,eslint,prettier}.config.*", Config, "build and tooling configuration"),
    hub("{.eslintrc*,.prettierrc*,.babelrc*,.nvmrc,.node-version}", Config, "lint, format and runtime policy"),
    hub("{src,app,pages}/{index,main,App,_app,layout,routes,router}.*", Registry, "app entry, routes and layout"),
    hub("**/src/{routes,router}/index.*", Registry, "route table"),
    hub("prisma/schema.prisma", Schema, "data model"),
    hub("{knexfile,drizzle.config}.*", Schema, "database tooling configuration"),
    hub("**/migrations/*", Schema, "migration slots"),
];

const GO: &[HubDef] = &[
    hub("go.mod", Manifest, "module and dependency requirements"),
    hub("{go.work,go.work.sum}", Manifest, "multi-module workspace"),
    hub("go.sum", Lockfile, "regenerate with go mod tidy"),
    hub("main.go", Registry, "wiring and flags"),
    hub("cmd/*/main.go", Registry, "binary wiring"),
    hub("**/{routes,router,wire,wire_gen,server,handlers}.go", Registry, "route and dependency wiring"),
    hub("**/*.proto", Schema, "wire schema"),
    hub("{sqlc.yaml,sqlc.yml,.golangci.yml,.golangci.yaml}", Config, "codegen and lint policy"),
    hub("**/migrations/*.sql", Schema, "migration slots"),
];

const CMAKE: &[HubDef] = &[
    hub("CMakeLists.txt", Manifest, "targets and options"),
    hub("**/CMakeLists.txt", Manifest, "subdirectory targets"),
    hub("{CMakePresets,CMakeUserPresets}.json", Config, "configure and build presets"),
    hub("{vcpkg.json,conanfile.txt,conanfile.py}", Manifest, "dependencies"),
    hub("cmake/*.cmake", Config, "shared modules"),
];

const MAKE: &[HubDef] = &[
    hub("{Makefile,GNUmakefile,makefile}", Manifest, "targets and recipes"),
    hub("**/*.mk", Config, "included make fragments"),
];

const COMPOSE: &[HubDef] = &[
    hub("{docker-compose,compose}{,.*}.{yml,yaml}", Manifest, "services, ports and volumes"),
    hub("**/Dockerfile*", Config, "image build"),
    hub("{.dockerignore,.env.example,.env.sample}", Config, "build context and variables"),
];

/// Patterns every repository shares, whatever its stacks.
const COMMON: &[HubDef] = &[
    hub(".github/workflows/*.{yml,yaml}", Config, "CI pipelines"),
    hub(".gitlab-ci.yml", Config, "CI pipeline"),
    hub(".pre-commit-config.yaml", Config, "commit hooks"),
    hub(".env.example", Config, "documented environment variables"),
    hub("{.editorconfig,.gitattributes}", Config, "editor and line-ending policy"),
    hub(".marlowe/project.toml", Config, "project profile override"),
];

/// The table for `stack`.
pub fn table(stack: Stack) -> &'static [HubDef] {
    match stack {
        Stack::Cargo => CARGO,
        Stack::Uv | Stack::Poetry | Stack::Pip => PYTHON,
        Stack::Npm | Stack::Pnpm | Stack::Yarn | Stack::Bun => NODE,
        Stack::Go => GO,
        Stack::Cmake => CMAKE,
        Stack::Make => MAKE,
        Stack::Compose => COMPOSE,
    }
}

/// The shared patterns.
pub fn common() -> &'static [HubDef] {
    COMMON
}

/// A hub file that exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubFile {
    /// Relative to the project root, forward slashes.
    pub path: String,
    pub class: HubClass,
    pub why: String,
}

/// Directory names never walked for hubs.
pub const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", "out", ".venv", "venv", "env", "__pycache__", ".next", ".nuxt",
    ".svelte-kit", ".cache", ".tox", ".mypy_cache", ".pytest_cache", ".ruff_cache", "vendor", "coverage", ".gradle",
    ".idea", ".vscode", "bin", "obj", ".turbo", ".parcel-cache",
];

/// Walks `root` once (depth 5, bounded) and returns every file matching one of `defs`, where each
/// `(prefix, defs)` entry anchors its patterns at `prefix` (a stack's directory, relative to
/// `root`; empty for the root). One file listed once, under the first pattern that matched.
pub fn resolve(ws: &dyn Workspace, root: &Path, sets: &[(String, &[HubDef])], extra: &[String]) -> Vec<HubFile> {
    let opts = WalkOpts {
        max_depth: Some(5),
        max_entries: Some(40_000),
        skip_dirs: SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
    };
    let Ok(entries) = ws.walk(root, &opts) else { return Vec::new() };
    let files: Vec<String> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| e.path.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let mut out: Vec<HubFile> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |path: &str, class: HubClass, why: &str| {
        if seen.insert(path.to_ascii_lowercase()) {
            out.push(HubFile { path: path.to_string(), class, why: why.to_string() });
        }
    };
    for (prefix, defs) in sets {
        for def in defs.iter() {
            let pattern = if prefix.is_empty() || prefix == "." {
                def.pattern.to_string()
            } else {
                format!("{}/{}", prefix.trim_end_matches('/'), def.pattern)
            };
            for f in files.iter().filter(|f| glob_match(&pattern, f)) {
                push(f, def.class, def.why);
            }
        }
    }
    for pattern in extra {
        for f in files.iter().filter(|f| glob_match(pattern, f)) {
            push(f, HubClass::Registry, "listed in .marlowe/project.toml");
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Matches `path` (forward slashes, relative) against `pattern`. See the module docs.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    expand_braces(pattern).iter().any(|p| match_segments(&p.split('/').collect::<Vec<_>>(), &path.split('/').collect::<Vec<_>>()))
}

/// Expands every `{a,b}` group (nesting is not supported).
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.to_string()] };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else { return vec![pattern.to_string()] };
    let (head, tail) = (&pattern[..open], &pattern[close + 1..]);
    let mut out = Vec::new();
    for alt in pattern[open + 1..close].split(',') {
        out.extend(expand_braces(&format!("{head}{alt}{tail}")));
    }
    out
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((seg, rest)) => match path.split_first() {
            Some((p, tail)) => match_segment(seg, p) && match_segments(rest, tail),
            None => false,
        },
    }
}

/// `*` and `?` within one segment, ASCII case-insensitive.
fn match_segment(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_anchor_cross_segments_and_alternate() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "crates/a/Cargo.toml"));
        assert!(glob_match("**/Cargo.toml", "Cargo.toml"));
        assert!(glob_match("**/Cargo.toml", "crates/a/Cargo.toml"));
        assert!(glob_match("crates/*/src/lib.rs", "crates/a/src/lib.rs"));
        assert!(!glob_match("crates/*/src/lib.rs", "crates/a/b/src/lib.rs"));
        assert!(glob_match("{rust-toolchain,rust-toolchain.toml}", "rust-toolchain.toml"));
        assert!(glob_match("tsconfig{,.*}.json", "tsconfig.json"));
        assert!(glob_match("tsconfig{,.*}.json", "tsconfig.build.json"));
        assert!(!glob_match("tsconfig{,.*}.json", "tsconfigx.json"));
        assert!(glob_match("makefile", "Makefile"), "case-insensitive");
        assert!(glob_match("requirements*.txt", "requirements-dev.txt"));
        assert!(glob_match("a?c", "abc") && !glob_match("a?c", "ac"));
    }

    #[test]
    fn every_stack_has_a_table_and_the_tables_parse() {
        for s in Stack::ALL {
            let t = table(s);
            assert!(t.len() >= 2, "{s:?} has hub patterns");
            for d in t.iter().chain(common()) {
                assert!(!d.pattern.is_empty() && !d.why.is_empty());
                assert!(!d.pattern.contains("{{"), "{}", d.pattern);
                assert!(expand_braces(d.pattern).iter().all(|p| !p.contains('{')), "{}", d.pattern);
            }
        }
    }
}
