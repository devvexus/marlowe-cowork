//! Per-stack detectors: manifests in, commands, services, migrations and lockfiles out.
//!
//! Detection reads files only. A command is listed when its manifest implies it (a `test` script,
//! a `[tool.ruff]` table, a `go.mod`), never because a tool might happen to be installed; whether
//! the program is on `PATH` is a separate flag on the command.

use std::path::Path;

use regex::Regex;
use serde_json::Value;

use crate::cmd::Cmd;
use crate::profile::{Found, Fs, Kind, Lockfile, Migrations, ProjectCmd, Script, ServiceDef, Source, Stack, StackAt};

/// Detects every stack in `dir` (`rel` is its path from the project root).
pub(crate) fn scan_dir(fs: &Fs<'_>, dir: &Path, rel: &str, f: &mut Found) {
    let names = fs.file_names(dir);
    let has = |n: &str| names.iter().any(|x| x.eq_ignore_ascii_case(n));
    if has("Cargo.toml") {
        cargo(fs, dir, rel, f);
    }
    if has("pyproject.toml") || has("setup.py") || has("setup.cfg") || names.iter().any(|n| is_requirements(n)) {
        python(fs, dir, rel, &names, f);
    }
    if has("package.json") {
        node(fs, dir, rel, &names, f);
    }
    if has("go.mod") {
        go(fs, dir, rel, &names, f);
    }
    if has("CMakeLists.txt") {
        cmake(rel, f);
    }
    if has("Makefile") || has("GNUmakefile") {
        make(fs, dir, rel, &names, f);
    }
    if names.iter().any(|n| is_compose(n)) {
        compose(rel, f);
    }
    crate::registries::refine(fs, dir, rel, f);
}

fn is_requirements(n: &str) -> bool {
    let n = n.to_ascii_lowercase();
    n.starts_with("requirements") && n.ends_with(".txt")
}

fn is_compose(n: &str) -> bool {
    matches!(
        n.to_ascii_lowercase().as_str(),
        "compose.yaml" | "compose.yml" | "docker-compose.yml" | "docker-compose.yaml"
    )
}

fn stack_at(f: &mut Found, stack: Stack, rel: &str) {
    f.stacks.push(StackAt { stack, dir: rel.to_string() });
}

fn lock(f: &mut Found, rel: &str, file: &str, stack: Stack, regen: &[&str]) {
    let file = if rel == "." { file.to_string() } else { format!("{rel}/{file}") };
    f.lockfiles.push(Lockfile { file, regen: Cmd::new(regen), stack });
}

fn python_exe() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

// ---- cargo -----------------------------------------------------------------------------------

fn cargo(fs: &Fs<'_>, dir: &Path, rel: &str, f: &mut Found) {
    stack_at(f, Stack::Cargo, rel);
    let manifest = fs.text(&dir.join("Cargo.toml")).unwrap_or_default();
    let ws = manifest.contains("[workspace]");
    let mut c = |kind: Kind, argv: &[&str]| {
        let argv: Vec<&str> = argv.iter().copied().filter(|a| !(a.is_empty())).collect();
        f.commands.push(ProjectCmd::new(kind, Stack::Cargo, rel, &argv));
    };
    let w = if ws { "--workspace" } else { "" };
    c(Kind::Typecheck, &["cargo", "check", w, "--all-targets"]);
    c(Kind::Build, &["cargo", "build", w]);
    c(Kind::Test, &["cargo", "test", w, "--no-fail-fast"]);
    c(Kind::Lint, &["cargo", "clippy", w, "--all-targets"]);
    c(Kind::Format, &["cargo", "fmt", "--all", "--", "--check"]);
    f.commands.push(ProjectCmd::new(Kind::Install, Stack::Cargo, rel, &["cargo", "fetch"]).registry("index.crates.io"));
    lock(f, rel, "Cargo.lock", Stack::Cargo, &["cargo", "update", "--workspace"]);
}

// ---- python ----------------------------------------------------------------------------------

fn python(fs: &Fs<'_>, dir: &Path, rel: &str, names: &[String], f: &mut Found) {
    let has = |n: &str| names.iter().any(|x| x.eq_ignore_ascii_case(n));
    let pyproject = fs.text(&dir.join("pyproject.toml")).unwrap_or_default();
    let mut reqs = String::new();
    let mut req_files: Vec<&String> = names.iter().filter(|n| is_requirements(n)).collect();
    req_files.sort();
    for n in &req_files {
        reqs.push_str(&fs.text(&dir.join(n)).unwrap_or_default().to_ascii_lowercase());
        reqs.push('\n');
    }
    let deps = format!("{}\n{}", pyproject.to_ascii_lowercase(), reqs);
    let uses = |word: &str| deps.contains(word);
    let stack = if has("uv.lock") || pyproject.contains("[tool.uv") {
        Stack::Uv
    } else if has("poetry.lock") || pyproject.contains("[tool.poetry]") {
        Stack::Poetry
    } else {
        Stack::Pip
    };
    stack_at(f, stack, rel);

    // How to run a tool inside the project's environment.
    let venv_py = ["Scripts/python.exe", "bin/python"].iter().find_map(|p| {
        let p = format!(".venv/{p}");
        fs.is_file(&dir.join(&p)).then_some(p)
    });
    let py = venv_py.unwrap_or_else(|| python_exe().to_string());
    let run = |tool: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = match stack {
            Stack::Uv => vec!["uv".into(), "run".into()],
            Stack::Poetry => vec!["poetry".into(), "run".into()],
            _ => Vec::new(),
        };
        v.extend(tool.iter().map(|s| s.to_string()));
        v
    };
    // `-m` form for pip projects (the environment's python), a direct tool call under uv/poetry.
    let tool = |name: &str, args: &[&str]| -> Vec<String> {
        let mut v = match stack {
            Stack::Pip => vec![py.clone(), "-m".into(), name.to_string()],
            _ => run(&[name]),
        };
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let mut add = |kind: Kind, argv: Vec<String>| {
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        f.commands.push(ProjectCmd::new(kind, stack, rel, &refs));
    };

    let has_tests_dir = fs.is_dir(&dir.join("tests")) || fs.is_dir(&dir.join("test"));
    let pytest = pyproject.contains("[tool.pytest")
        || has("pytest.ini")
        || has("conftest.py")
        || uses("pytest")
        || (has("tox.ini") && fs.text(&dir.join("tox.ini")).is_some_and(|t| t.contains("[pytest]")));
    if pytest {
        add(Kind::Test, tool("pytest", &["-q"]));
    } else if has_tests_dir {
        let mut v = match stack {
            Stack::Pip => vec![py.clone()],
            _ => run(&["python"]),
        };
        v.extend(["-m", "unittest", "discover"].map(String::from));
        add(Kind::Test, v);
    }
    if pyproject.contains("[tool.ruff") || has("ruff.toml") || has(".ruff.toml") || uses("ruff") {
        add(Kind::Lint, tool("ruff", &["check", "."]));
        add(Kind::Format, tool("ruff", &["format", "--check", "."]));
    } else if pyproject.contains("[tool.black]") || uses("black") {
        add(Kind::Format, tool("black", &["--check", "."]));
    }
    if has("pyrightconfig.json") || pyproject.contains("[tool.pyright]") || uses("pyright") {
        add(Kind::Typecheck, run(&["pyright"]));
    } else if pyproject.contains("[tool.mypy]") || has("mypy.ini") || uses("mypy") {
        add(Kind::Typecheck, tool("mypy", &["."]));
    }
    match stack {
        Stack::Uv => {
            add(Kind::Install, vec!["uv".into(), "sync".into()]);
            if pyproject.contains("[build-system]") {
                add(Kind::Build, vec!["uv".into(), "build".into()]);
            }
            lock(f, rel, "uv.lock", Stack::Uv, &["uv", "lock"]);
        }
        Stack::Poetry => {
            add(Kind::Install, vec!["poetry".into(), "install".into()]);
            if pyproject.contains("[build-system]") {
                add(Kind::Build, vec!["poetry".into(), "build".into()]);
            }
            lock(f, rel, "poetry.lock", Stack::Poetry, &["poetry", "lock"]);
        }
        _ => {
            if let Some(r) = req_files.iter().find(|n| n.eq_ignore_ascii_case("requirements.txt")).or(req_files.first()) {
                add(Kind::Install, vec![py.clone(), "-m".into(), "pip".into(), "install".into(), "-r".into(), r.to_string()]);
            } else if has("pyproject.toml") || has("setup.py") {
                add(Kind::Install, vec![py.clone(), "-m".into(), "pip".into(), "install".into(), "-e".into(), ".".into()]);
            }
        }
    }
    for c in f.commands.iter_mut().filter(|c| c.kind == Kind::Install && c.stack == Some(stack) && c.cwd == rel) {
        c.registry_host = Some("pypi.org".into());
    }

    // Services and migrations.
    if has("manage.py") {
        let mut argv = run(&["python"]);
        if stack == Stack::Pip {
            argv = vec![py.clone()];
        }
        let mk = |tail: &[&str]| -> Cmd {
            let mut v = argv.clone();
            v.push("manage.py".into());
            v.extend(tail.iter().map(|s| s.to_string()));
            Cmd { argv: v }
        };
        f.services.push(service(
            "django",
            mk(&["runserver", "127.0.0.1:8000", "--noreload"]),
            rel,
            Some("http://127.0.0.1:8000/"),
            Some(8000),
        ));
        f.migrations.push(Migrations {
            tool: "django".into(),
            up: mk(&["migrate"]),
            down: None,
            cwd: rel.to_string(),
            config_files: Vec::new(),
            env: vec![("DATABASE_URL".into(), "{db_url}".into())],
            source: Source::Detected,
            available: true,
        });
    } else if uses("fastapi") || uses("uvicorn") {
        let target = if fs.is_file(&dir.join("main.py")) {
            Some("main:app")
        } else if fs.is_file(&dir.join("app").join("main.py")) {
            Some("app.main:app")
        } else {
            None
        };
        if let Some(t) = target {
            let mut argv = match stack {
                Stack::Pip => vec![py.clone(), "-m".into(), "uvicorn".into()],
                _ => run(&["uvicorn"]),
            };
            argv.extend([t.to_string(), "--host".into(), "127.0.0.1".into(), "--port".into(), "8000".into()]);
            f.services.push(service("api", Cmd { argv }, rel, Some("http://127.0.0.1:8000/"), Some(8000)));
        }
    } else if uses("flask") && fs.is_file(&dir.join("app.py")) {
        let mut argv = match stack {
            Stack::Pip => vec![py.clone(), "-m".into(), "flask".into()],
            _ => run(&["flask"]),
        };
        argv.extend(["--app", "app", "run", "--host", "127.0.0.1", "--port", "5000"].map(String::from));
        f.services.push(service("flask", Cmd { argv }, rel, Some("http://127.0.0.1:5000/"), Some(5000)));
    }
    if has("alembic.ini") {
        let a = |tail: &[&str]| -> Cmd {
            let mut v = match stack {
                Stack::Pip => vec![py.clone(), "-m".into(), "alembic".into()],
                _ => run(&["alembic"]),
            };
            v.extend(tail.iter().map(|s| s.to_string()));
            Cmd { argv: v }
        };
        f.migrations.push(Migrations {
            tool: "alembic".into(),
            up: a(&["upgrade", "head"]),
            down: Some(a(&["downgrade", "base"])),
            cwd: rel.to_string(),
            config_files: vec!["alembic.ini".into()],
            env: vec![("DATABASE_URL".into(), "{db_url}".into())],
            source: Source::Detected,
            available: true,
        });
    }
}

fn service(name: &str, cmd: Cmd, rel: &str, url: Option<&str>, port: Option<u16>) -> ServiceDef {
    ServiceDef {
        name: name.into(),
        cmd,
        cwd: rel.to_string(),
        ready_url: url.map(String::from),
        ready_port: port,
        ready_log: None,
        env_allow: Vec::new(),
        source: Source::Detected,
        available: true,
        runs: Vec::new(),
    }
}

// ---- node ------------------------------------------------------------------------------------

fn node(fs: &Fs<'_>, dir: &Path, rel: &str, names: &[String], f: &mut Found) {
    let has = |n: &str| names.iter().any(|x| x.eq_ignore_ascii_case(n));
    let pkg: Value = fs.text(&dir.join("package.json")).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let manager = pkg.get("packageManager").and_then(Value::as_str).unwrap_or("");
    let stack = if manager.starts_with("bun") || has("bun.lockb") || has("bun.lock") {
        Stack::Bun
    } else if manager.starts_with("pnpm") || has("pnpm-lock.yaml") || has("pnpm-workspace.yaml") {
        Stack::Pnpm
    } else if manager.starts_with("yarn") || has("yarn.lock") || has(".yarnrc.yml") {
        Stack::Yarn
    } else {
        Stack::Npm
    };
    stack_at(f, stack, rel);
    let pm = stack.name();
    let scripts = pkg.get("scripts").and_then(Value::as_object);
    let script = |names: &[&str]| -> Option<String> {
        let s = scripts?;
        names
            .iter()
            .find(|n| s.get(**n).and_then(Value::as_str).is_some_and(|body| !body.contains("no test specified")))
            .map(|n| n.to_string())
    };
    let dep = |name: &str| {
        ["dependencies", "devDependencies"].iter().any(|k| pkg.get(*k).and_then(|d| d.get(name)).is_some())
    };
    let sep = if stack == Stack::Npm { Some("--") } else { None };
    let mut add = |kind: Kind, argv: &[&str]| {
        let mut c = ProjectCmd::new(kind, stack, rel, argv);
        if let (Some(s), true) = (sep, argv.len() >= 2 && argv[1] == "run") {
            c = c.separator(s);
        }
        c.runs = node_runs(&pkg, &argv.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        f.commands.push(c);
    };
    for (kind, candidates) in [
        (Kind::Test, &["test", "test:unit", "unit"][..]),
        (Kind::Build, &["build"][..]),
        (Kind::Lint, &["lint"][..]),
        (Kind::Format, &["format:check", "prettier:check", "check-format", "format-check", "fmt:check"][..]),
    ] {
        if let Some(s) = script(candidates) {
            add(kind, &[pm, "run", &s]);
        }
    }
    if let Some(s) = script(&["typecheck", "type-check", "check-types", "tsc", "types"]) {
        add(Kind::Typecheck, &[pm, "run", &s]);
    } else if has("tsconfig.json") && dep("typescript") {
        let exec: &[&str] = match stack {
            Stack::Npm => &["npx", "--no-install", "tsc", "--noEmit"],
            Stack::Pnpm => &["pnpm", "exec", "tsc", "--noEmit"],
            Stack::Yarn => &["yarn", "tsc", "--noEmit"],
            _ => &["bunx", "tsc", "--noEmit"],
        };
        add(Kind::Typecheck, exec);
    }
    let install: &[&str] = match stack {
        Stack::Npm if has("package-lock.json") => &["npm", "ci"],
        Stack::Npm => &["npm", "install"],
        Stack::Pnpm => &["pnpm", "install"],
        Stack::Yarn => &["yarn", "install"],
        _ => &["bun", "install"],
    };
    let host = if stack == Stack::Yarn && !has(".yarnrc.yml") { "registry.yarnpkg.com" } else { "registry.npmjs.org" };
    let mut inst = ProjectCmd::new(Kind::Install, stack, rel, install).registry(host);
    // The root's own lifecycle scripts (`postinstall`, `prepare`) run on every install.
    inst.runs = node_runs(&pkg, &install.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    f.commands.push(inst);
    match stack {
        Stack::Npm => lock(f, rel, "package-lock.json", stack, &["npm", "install", "--package-lock-only"]),
        Stack::Pnpm => lock(f, rel, "pnpm-lock.yaml", stack, &["pnpm", "install", "--lockfile-only"]),
        Stack::Yarn => lock(f, rel, "yarn.lock", stack, &["yarn", "install", "--mode", "update-lockfile"]),
        _ => lock(f, rel, if has("bun.lock") { "bun.lock" } else { "bun.lockb" }, stack, &["bun", "install", "--lockfile-only"]),
    }

    // A dev server, with the port its framework defaults to.
    let framework: Option<(&str, u16)> = [
        ("vite", 5173u16),
        ("next", 3000),
        ("react-scripts", 3000),
        ("nuxt", 3000),
        ("astro", 4321),
        ("@sveltejs/kit", 5173),
        ("@angular/cli", 4200),
        ("webpack-dev-server", 8080),
        ("parcel", 1234),
        ("gatsby", 8000),
        ("@remix-run/dev", 3000),
    ]
    .into_iter()
    .find(|(d, _)| dep(d));
    if let Some(s) = script(&["dev", "start", "serve"]) {
        let url = framework.map(|(_, p)| format!("http://localhost:{p}/"));
        let mut def = service("dev", Cmd::new(&[pm, "run", &s]), rel, url.as_deref(), framework.map(|(_, p)| p));
        def.runs = node_runs(&pkg, &[pm.to_string(), "run".to_string(), s.clone()]);
        f.services.push(def);
    }

    // Migrations.
    let exec = |tail: &[&str]| -> Cmd {
        let mut v: Vec<&str> = match stack {
            Stack::Npm => vec!["npx", "--no-install"],
            Stack::Pnpm => vec!["pnpm", "exec"],
            Stack::Yarn => vec!["yarn"],
            _ => vec!["bunx"],
        };
        v.extend(tail);
        Cmd::new(&v)
    };
    let mig = |tool: &str, up: Cmd, down: Option<Cmd>, cfg: &[&str]| Migrations {
        tool: tool.into(),
        up,
        down,
        cwd: rel.to_string(),
        config_files: cfg.iter().map(|s| s.to_string()).collect(),
        env: vec![("DATABASE_URL".into(), "{db_url}".into())],
        source: Source::Detected,
        available: true,
    };
    if fs.is_file(&dir.join("prisma").join("schema.prisma")) {
        let mut m = mig("prisma", exec(&["prisma", "migrate", "deploy"]), None, &["prisma/schema.prisma"]);
        m.env = vec![("DATABASE_URL".into(), "file:{db_path}".into())];
        f.migrations.push(m);
    }
    if let Some(k) = names.iter().find(|n| n.to_ascii_lowercase().starts_with("knexfile.")) {
        f.migrations.push(mig(
            "knex",
            exec(&["knex", "migrate:latest"]),
            Some(exec(&["knex", "migrate:rollback", "--all"])),
            &[k.as_str()],
        ));
    }
    if let Some(k) = names.iter().find(|n| n.to_ascii_lowercase().starts_with("drizzle.config.")) {
        f.migrations.push(mig("drizzle", exec(&["drizzle-kit", "migrate"]), None, &[k.as_str()]));
    }
    if has(".sequelizerc") {
        f.migrations.push(mig(
            "sequelize",
            exec(&["sequelize-cli", "db:migrate"]),
            Some(exec(&["sequelize-cli", "db:migrate:undo:all"])),
            &[".sequelizerc"],
        ));
    }
}

/// The scripts a package-manager command runs (see [`Script`]), from `pkg` (a parsed
/// package.json): the named script with its `pre`/`post` hooks and every script those call
/// (`npm run lint && jest`), or for an install the root's lifecycle scripts. Empty when the
/// command does not run package scripts (`npx tsc`, `cargo test`).
pub(crate) fn node_runs(pkg: &Value, argv: &[String]) -> Vec<Script> {
    const INSTALL_HOOKS: [&str; 7] = ["preinstall", "install", "postinstall", "prepublish", "preprepare", "prepare", "postprepare"];
    let Some(scripts) = pkg.get("scripts").and_then(Value::as_object) else { return Vec::new() };
    let prog = argv.first().map(|p| Path::new(p).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()).unwrap_or_default();
    if !matches!(prog.as_str(), "npm" | "pnpm" | "yarn" | "bun") {
        return Vec::new();
    }
    let rest: Vec<&str> = argv.iter().skip(1).map(String::as_str).filter(|a| !a.starts_with('-')).collect();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    match rest.first().copied() {
        Some("run" | "run-script" | "rum" | "urn") => {
            if let Some(name) = rest.get(1) {
                collect_script(scripts, name, true, &mut seen, &mut out, 0);
            }
        }
        Some("test" | "t" | "tst") => collect_script(scripts, "test", true, &mut seen, &mut out, 0),
        Some("start") => collect_script(scripts, "start", true, &mut seen, &mut out, 0),
        Some("stop") => collect_script(scripts, "stop", true, &mut seen, &mut out, 0),
        Some("install" | "i" | "ci" | "add" | "update" | "up") => {
            for hook in INSTALL_HOOKS {
                take_script(scripts, hook, false, &mut seen, &mut out, 0);
            }
        }
        None if prog == "yarn" => {
            for hook in INSTALL_HOOKS {
                take_script(scripts, hook, false, &mut seen, &mut out, 0);
            }
        }
        // `yarn build` is a script.
        Some(name) if prog == "yarn" && scripts.contains_key(name) => collect_script(scripts, name, true, &mut seen, &mut out, 0),
        _ => {}
    }
    out
}

/// [`node_runs`] for the package.json in `dir`.
pub(crate) fn node_runs_at(fs: &Fs<'_>, dir: &Path, argv: &[String]) -> Vec<Script> {
    let Some(pkg) = fs.text(&dir.join("package.json")).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return Vec::new() };
    node_runs(&pkg, argv)
}

/// `name` and the `pre`/`post` scripts the package manager runs around it.
fn collect_script(scripts: &serde_json::Map<String, Value>, name: &str, own: bool, seen: &mut std::collections::HashSet<String>, out: &mut Vec<Script>, depth: usize) {
    take_script(scripts, &format!("pre{name}"), false, seen, out, depth);
    take_script(scripts, name, own, seen, out, depth);
    take_script(scripts, &format!("post{name}"), false, seen, out, depth);
}

/// One script by exact name (`own`: it is the one the command names, so it carries no label),
/// then the scripts its body calls.
fn take_script(scripts: &serde_json::Map<String, Value>, name: &str, own: bool, seen: &mut std::collections::HashSet<String>, out: &mut Vec<Script>, depth: usize) {
    static CALLS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let calls = CALLS.get_or_init(|| {
        Regex::new(r"\b(?:npm|pnpm|bun)\s+(?:run|run-script)\s+(?:-\S+\s+)*([A-Za-z0-9:_.\-]+)|\byarn\s+(?:run\s+)?([A-Za-z0-9:_.\-]+)|\bnpm\s+(test|start|stop)\b").expect("static regex")
    });
    if depth > 4 || out.len() >= 24 || !seen.insert(name.to_string()) {
        return;
    }
    let Some(body) = scripts.get(name).and_then(Value::as_str) else { return };
    out.push(Script { name: if own { String::new() } else { name.to_string() }, body: body.to_string() });
    let called: Vec<String> = calls.captures_iter(body).filter_map(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().to_string())).collect();
    for next in called {
        if scripts.contains_key(&next) {
            collect_script(scripts, &next, false, seen, out, depth + 1);
        }
    }
}

// ---- go --------------------------------------------------------------------------------------

fn go(fs: &Fs<'_>, dir: &Path, rel: &str, names: &[String], f: &mut Found) {
    stack_at(f, Stack::Go, rel);
    f.commands.push(ProjectCmd::new(Kind::Build, Stack::Go, rel, &["go", "build", "./..."]));
    f.commands.push(ProjectCmd::new(Kind::Test, Stack::Go, rel, &["go", "test", "./..."]));
    f.commands.push(ProjectCmd::new(Kind::Typecheck, Stack::Go, rel, &["go", "vet", "./..."]));
    let has = |n: &str| names.iter().any(|x| x.eq_ignore_ascii_case(n));
    if has(".golangci.yml") || has(".golangci.yaml") {
        f.commands.push(ProjectCmd::new(Kind::Lint, Stack::Go, rel, &["golangci-lint", "run"]));
    }
    let mut fmt = ProjectCmd::new(Kind::Format, Stack::Go, rel, &["gofmt", "-l", "."]);
    fmt.fail_on_output = true;
    f.commands.push(fmt);
    f.commands.push(ProjectCmd::new(Kind::Install, Stack::Go, rel, &["go", "mod", "download"]).registry("proxy.golang.org"));
    lock(f, rel, "go.sum", Stack::Go, &["go", "mod", "tidy"]);
    // goose / golang-migrate layouts.
    let mdir = dir.join("migrations");
    if fs.is_dir(&mdir) {
        let files = fs.file_names(&mdir);
        let migrate_style = files.iter().any(|n| n.ends_with(".up.sql"));
        let goose_style = files.iter().any(|n| {
            n.ends_with(".sql")
                && fs.text(&mdir.join(n)).is_some_and(|t| t.contains("+goose Up"))
        });
        if migrate_style {
            f.migrations.push(Migrations {
                tool: "golang-migrate".into(),
                up: Cmd::new(&["migrate", "-path", "migrations", "-database", "{db_url}", "up"]),
                down: Some(Cmd::new(&["migrate", "-path", "migrations", "-database", "{db_url}", "down", "-all"])),
                cwd: rel.to_string(),
                config_files: Vec::new(),
                env: vec![("DATABASE_URL".into(), "{db_url}".into())],
                source: Source::Detected,
                available: true,
            });
        } else if goose_style {
            f.migrations.push(Migrations {
                tool: "goose".into(),
                up: Cmd::new(&["goose", "-dir", "migrations", "sqlite3", "{db_path}", "up"]),
                down: Some(Cmd::new(&["goose", "-dir", "migrations", "sqlite3", "{db_path}", "down-to", "0"])),
                cwd: rel.to_string(),
                config_files: Vec::new(),
                env: vec![("DATABASE_URL".into(), "{db_url}".into())],
                source: Source::Detected,
                available: true,
            });
        }
    }
}

// ---- cmake, make, compose ---------------------------------------------------------------------

fn cmake(rel: &str, f: &mut Found) {
    stack_at(f, Stack::Cmake, rel);
    let configure = Cmd::new(&["cmake", "-S", ".", "-B", "build"]);
    let mut build = ProjectCmd::new(Kind::Build, Stack::Cmake, rel, &["cmake", "--build", "build"]);
    build.pre = Some((configure.clone(), "build/CMakeCache.txt".into()));
    f.commands.push(build);
    let mut test = ProjectCmd::new(Kind::Test, Stack::Cmake, rel, &["ctest", "--test-dir", "build", "--output-on-failure"]);
    test.pre = Some((configure, "build/CMakeCache.txt".into()));
    f.commands.push(test);
}

fn make(fs: &Fs<'_>, dir: &Path, rel: &str, names: &[String], f: &mut Found) {
    stack_at(f, Stack::Make, rel);
    let file = ["Makefile", "GNUmakefile", "makefile"]
        .iter()
        .find(|n| names.iter().any(|x| x == **n))
        .copied()
        .unwrap_or("Makefile");
    let text = fs.text(&dir.join(file)).unwrap_or_default();
    let re = Regex::new(r"(?m)^([A-Za-z0-9_][A-Za-z0-9_./-]*)[ \t]*:([^=]|$)").expect("static regex");
    let targets: Vec<String> = re.captures_iter(&text).map(|c| c[1].to_string()).collect();
    let target = |names: &[&str]| names.iter().find(|n| targets.iter().any(|t| t == **n)).map(|n| n.to_string());
    for (kind, candidates) in [
        (Kind::Build, &["build", "all"][..]),
        (Kind::Test, &["test", "tests", "check"][..]),
        (Kind::Lint, &["lint"][..]),
        (Kind::Format, &["format-check", "fmt-check", "format", "fmt"][..]),
        (Kind::Typecheck, &["typecheck", "check-types"][..]),
        (Kind::Install, &["deps", "setup", "bootstrap"][..]),
    ] {
        if let Some(t) = target(candidates) {
            f.commands.push(ProjectCmd::new(kind, Stack::Make, rel, &["make", &t]));
        }
    }
    if let Some(t) = target(&["dev", "run", "serve", "start"]) {
        f.services.push(service(&t, Cmd::new(&["make", &t]), rel, None, None));
    }
}

fn compose(rel: &str, f: &mut Found) {
    stack_at(f, Stack::Compose, rel);
    f.commands.push(ProjectCmd::new(Kind::Lint, Stack::Compose, rel, &["docker", "compose", "config", "--quiet"]));
    f.commands.push(ProjectCmd::new(Kind::Build, Stack::Compose, rel, &["docker", "compose", "build"]));
    f.services.push(service("compose", Cmd::new(&["docker", "compose", "up"]), rel, None, None));
}
