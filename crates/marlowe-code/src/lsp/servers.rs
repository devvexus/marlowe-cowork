//! Which language server serves which language, and where its program is.
//!
//! | Server | Languages | Command |
//! |---|---|---|
//! | `rust-analyzer` | Rust | `rust-analyzer` (rustup component) |
//! | `pyright` | Python | `pyright-langserver --stdio` (npm) |
//! | `typescript-language-server` | TypeScript, TSX, JavaScript | `typescript-language-server --stdio` (npm), when a `tsserver.js` exists (the workspace's TypeScript, else a global one before 7) |
//! | `typescript` | TypeScript, TSX, JavaScript | TypeScript 7's native server, `tsc --lsp --stdio` (its platform executable, started directly) |
//!
//! A language's first server that can run for the root serves it. Programs are found on `PATH`
//! (with `PATHEXT`, so npm's `.cmd` shims resolve), then in `%APPDATA%\npm` and `~/.cargo/bin`,
//! which a daemon started outside a login shell may not have on its `PATH`. rust-analyzer's
//! `cargo check` runs in its own target subdirectory (`cargo.targetDir`), so it never holds the
//! lock a user's build waits on.
//!
//! **Trust.** A server is started *trusted* only for the user's own free-write workspace in a
//! session no untrusted content has entered; a session that took in untrusted content starts no
//! server at all (it writes the workspace freely, and servers read config there). Untrusted
//! (another workspace, untainted): the TypeScript servers use the global TypeScript, never
//! `<root>/node_modules`; rust-analyzer starts with build scripts, proc macros and `checkOnSave`
//! (a `cargo check`, which runs both) off; and the start is refused when the root has config that
//! names a program the server would run ([`ServerSpec::untrusted_hazard`]: `.cargo/config*` and
//! `rust-toolchain*` for rust-analyzer's `cargo metadata`, `rust-analyzer.toml`, pyright's
//! interpreter settings). Trusted, rust-analyzer runs all three, so post-edit diagnostics include
//! rustc's own errors (borrowck, lifetimes, trait bounds).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::index::Lang;

/// `initializationOptions` for a root, trusted or not.
pub type OptionsFn = fn(&Path, bool, &Value) -> Value;
/// The command for a root, trusted or not (`None`: cannot run there).
pub type CommandFn = fn(&Path, bool) -> Option<Vec<String>>;
/// What in a root would make the server run a program the workspace names (`None`: nothing).
pub type HazardFn = fn(&Path) -> Option<String>;

/// One language server.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// Stable id (`rust-analyzer`, `pyright`, `typescript-language-server`, or a test's).
    pub id: &'static str,
    pub langs: Vec<Lang>,
    /// Program name or path, then arguments.
    pub command: Vec<String>,
    /// `initializationOptions` (for a trusted root; [`Self::options_for_root`] adjusts them).
    pub init_options: Value,
    /// Extra environment for the child (after the credential scrub).
    pub env: Vec<(&'static str, &'static str)>,
    /// Files whose presence at the workspace root says this server is wanted there, so an edit
    /// starts it (the `diagnostics` tool and the symbol tools start it regardless).
    pub markers: Vec<&'static str>,
    /// The server reports `experimental/serverStatus` (rust-analyzer): requests wait for it to
    /// be quiescent after start.
    pub server_status: bool,
    /// `initializationOptions` for a given root and trust, when they depend on them.
    pub options_for_root: Option<OptionsFn>,
    /// The command for a given root and trust, when it depends on them (`None`: cannot run).
    pub command_for_root: Option<CommandFn>,
    /// Workspace config that would make the server run a program the workspace names; an
    /// untrusted start is refused when it is present.
    pub hazard: Option<HazardFn>,
}

impl ServerSpec {
    pub fn serves(&self, lang: Lang) -> bool {
        self.langs.contains(&lang)
    }

    /// The command to run for `root` with its program resolved; `None` when it cannot run.
    pub fn launch_command(&self, root: &Path, trusted: bool) -> Option<Vec<String>> {
        if let Some(f) = self.command_for_root {
            return f(root, trusted);
        }
        let program = find_program(self.command.first()?)?;
        Some(std::iter::once(program.to_string_lossy().into_owned()).chain(self.command[1..].iter().cloned()).collect())
    }

    /// The `initializationOptions` to send for `root`.
    pub fn init_options_for(&self, root: &Path, trusted: bool) -> Value {
        match self.options_for_root {
            Some(f) => f(root, trusted, &self.init_options),
            None => self.init_options.clone(),
        }
    }

    /// Why an untrusted start in `root` is refused: config there names a program to run.
    pub fn untrusted_hazard(&self, root: &Path) -> Option<String> {
        self.hazard.and_then(|f| f(root))
    }

    /// Whether `root` has one of this server's project markers.
    pub fn wanted_in(&self, root: &Path) -> bool {
        self.markers.iter().any(|m| root.join(m).is_file())
    }
}

const TS_MARKERS: [&str; 3] = ["package.json", "tsconfig.json", "jsconfig.json"];

/// rust-analyzer, pyright and the two TypeScript servers.
pub fn default_specs() -> Vec<ServerSpec> {
    vec![
        ServerSpec {
            id: "rust-analyzer",
            langs: vec![Lang::Rust],
            command: vec!["rust-analyzer".into()],
            init_options: json!({
                "cargo": {"targetDir": true, "buildScripts": {"enable": true}},
                "procMacro": {"enable": true},
                "checkOnSave": true,
                "check": {"command": "check"},
                "files": {"watcher": "server"},
            }),
            env: Vec::new(),
            markers: vec!["Cargo.toml"],
            server_status: true,
            options_for_root: Some(rust_analyzer_options),
            command_for_root: None,
            hazard: Some(rust_hazard),
        },
        ServerSpec {
            id: "pyright",
            langs: vec![Lang::Python],
            command: vec!["pyright-langserver".into(), "--stdio".into()],
            init_options: json!({}),
            env: Vec::new(),
            markers: vec!["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", "pyrightconfig.json"],
            server_status: false,
            options_for_root: None,
            command_for_root: None,
            hazard: Some(pyright_hazard),
        },
        ServerSpec {
            id: "typescript-language-server",
            langs: vec![Lang::TypeScript, Lang::Tsx, Lang::JavaScript],
            command: vec!["typescript-language-server".into(), "--stdio".into()],
            init_options: json!({"hostInfo": "marlowe"}),
            env: Vec::new(),
            markers: TS_MARKERS.to_vec(),
            server_status: false,
            options_for_root: Some(tsserver_options),
            command_for_root: Some(tls_command),
            hazard: None,
        },
        ServerSpec {
            id: "typescript",
            langs: vec![Lang::TypeScript, Lang::Tsx, Lang::JavaScript],
            command: vec!["tsc".into(), "--lsp".into(), "--stdio".into()],
            init_options: json!({}),
            env: Vec::new(),
            markers: TS_MARKERS.to_vec(),
            server_status: false,
            options_for_root: None,
            command_for_root: Some(native_ts_command),
            hazard: None,
        },
    ]
}

/// The first of `names` present in `root` or one of its immediate subfolders (rust-analyzer
/// discovers Cargo projects one level down, and cargo reads config from where it runs).
fn first_present(root: &Path, names: &[&str]) -> Option<PathBuf> {
    let subdirs = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path());
    std::iter::once(root.to_path_buf()).chain(subdirs).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.exists())
}

/// rust-analyzer runs `cargo metadata` (through the rustup proxy) even with build scripts off:
/// `.cargo/config*` can name `build.rustc`/`rustc-wrapper`, `rust-toolchain*` a toolchain path,
/// and `rust-analyzer.toml` overrides its settings.
pub fn rust_hazard(root: &Path) -> Option<String> {
    first_present(root, &[".cargo/config", ".cargo/config.toml", "rust-toolchain", "rust-toolchain.toml", "rust-analyzer.toml"])
        .map(|p| p.display().to_string())
}

/// pyright runs the interpreter its config names (`venvPath`/`venv`, `pythonPath`) to find
/// search paths: `pyrightconfig.json`, or a `pyproject.toml` with a `[tool.pyright]` table.
pub fn pyright_hazard(root: &Path) -> Option<String> {
    if let Some(p) = first_present(root, &["pyrightconfig.json"]) {
        return Some(p.display().to_string());
    }
    let pyproject = root.join("pyproject.toml");
    let text = std::fs::read_to_string(&pyproject).ok()?;
    text.lines().any(|l| l.trim_start().starts_with("[tool.pyright")).then(|| pyproject.display().to_string())
}

/// Untrusted, rust-analyzer runs nothing the workspace supplies: no build scripts, no proc
/// macros, no `cargo check` on save.
fn rust_analyzer_options(_root: &Path, trusted: bool, base: &Value) -> Value {
    let mut opts = base.clone();
    if !trusted {
        opts["cargo"]["buildScripts"]["enable"] = json!(false);
        opts["procMacro"]["enable"] = json!(false);
        opts["checkOnSave"] = json!(false);
    }
    opts
}

/// The TypeScript package a root uses: when trusted, its own `node_modules/typescript` first;
/// else (and failing that) the global npm one beside `tsc`. With its major version.
fn typescript_package(root: &Path, trusted: bool) -> Option<(PathBuf, u64)> {
    let local = trusted.then(|| root.join("node_modules").join("typescript"));
    let global = find_program("tsc").and_then(|tsc| tsc.parent().map(|d| d.join("node_modules").join("typescript")));
    for dir in local.into_iter().chain(global) {
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { continue };
        let Ok(pkg) = serde_json::from_str::<Value>(&text) else { continue };
        let major = pkg.get("version").and_then(Value::as_str).and_then(|v| v.split('.').next()?.parse().ok()).unwrap_or(0);
        return Some((dir, major));
    }
    None
}

/// `tsserver.js` of the root's TypeScript (before 7, which has no tsserver).
fn tsserver_js(root: &Path, trusted: bool) -> Option<PathBuf> {
    let (dir, _) = typescript_package(root, trusted)?;
    Some(dir.join("lib").join("tsserver.js")).filter(|p| p.is_file())
}

fn tls_command(root: &Path, trusted: bool) -> Option<Vec<String>> {
    tsserver_js(root, trusted)?;
    let program = find_program("typescript-language-server")?;
    Some(vec![program.to_string_lossy().into_owned(), "--stdio".into()])
}

/// typescript-language-server is told which tsserver to use.
fn tsserver_options(root: &Path, trusted: bool, base: &Value) -> Value {
    let mut opts = base.clone();
    if let Some(js) = tsserver_js(root, trusted) {
        opts["tsserver"] = json!({"path": js.to_string_lossy()});
    }
    opts
}

/// TypeScript 7's platform executable (`@typescript/typescript-<platform>-<arch>/lib/tsc.exe`).
fn native_ts_command(root: &Path, trusted: bool) -> Option<Vec<String>> {
    let (dir, major) = typescript_package(root, trusted)?;
    if major < 7 {
        return None;
    }
    let (platform, arch, exe) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => ("win32", "x64", "tsc.exe"),
        ("windows", "aarch64") => ("win32", "arm64", "tsc.exe"),
        ("linux", "x86_64") => ("linux", "x64", "tsc"),
        ("linux", "aarch64") => ("linux", "arm64", "tsc"),
        ("macos", "x86_64") => ("darwin", "x64", "tsc"),
        ("macos", "aarch64") => ("darwin", "arm64", "tsc"),
        _ => return None,
    };
    let bin = dir.join("node_modules").join("@typescript").join(format!("typescript-{platform}-{arch}")).join("lib").join(exe);
    bin.is_file().then(|| vec![bin.to_string_lossy().into_owned(), "--lsp".into(), "--stdio".into()])
}

/// `name` as a runnable path: itself when it is a path that exists, else the first match on
/// `PATH` (trying each `PATHEXT` extension on Windows), else the npm and cargo bin folders.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        dirs.push(PathBuf::from(home).join(".cargo").join("bin"));
    }
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        pathext.split(';').filter(|e| !e.is_empty()).map(|e| e.to_ascii_lowercase()).collect()
    } else {
        vec![String::new()]
    };
    for dir in dirs {
        for ext in &exts {
            // `.ps1` shims cannot be started directly.
            if ext == ".ps1" {
                continue;
            }
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_language_has_a_server_and_programs_resolve() {
        let specs = default_specs();
        for lang in [Lang::Rust, Lang::Python, Lang::TypeScript, Lang::Tsx, Lang::JavaScript] {
            assert!(specs.iter().any(|s| s.serves(lang)), "{lang:?}");
        }
        assert!(specs.iter().all(|s| !s.serves(Lang::Go) && !s.serves(Lang::C)));
        assert_eq!(find_program("definitely-not-a-program-marlowe"), None);
        let me = std::env::current_exe().unwrap();
        assert_eq!(find_program(me.to_str().unwrap()), Some(me));
        // A workspace TypeScript before 7 is served by typescript-language-server through its
        // tsserver; the native server is not offered for it.
        let root = std::env::temp_dir().join(format!("marlowe-ts-{}", std::process::id()));
        let lib = root.join("node_modules").join("typescript").join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(root.join("node_modules/typescript/package.json"), r#"{"version": "5.6.3"}"#).unwrap();
        std::fs::write(lib.join("tsserver.js"), "").unwrap();
        assert_eq!(typescript_package(&root, true).map(|p| p.1), Some(5));
        assert_eq!(native_ts_command(&root, true), None);
        assert_eq!(tsserver_options(&root, true, &json!({}))["tsserver"]["path"], json!(lib.join("tsserver.js").to_string_lossy()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_untrusted_root_runs_nothing_the_workspace_supplies() {
        // A workspace TypeScript, with a planted native executable, is never used untrusted.
        let root = std::env::temp_dir().join(format!("marlowe-ts-untrusted-{}", std::process::id()));
        let ts = root.join("node_modules").join("typescript");
        std::fs::create_dir_all(ts.join("lib")).unwrap();
        std::fs::write(ts.join("package.json"), r#"{"version": "7.0.0"}"#).unwrap();
        std::fs::write(ts.join("lib").join("tsserver.js"), "").unwrap();
        for arch in ["x64", "arm64"] {
            let exe = ts.join("node_modules").join("@typescript").join(format!("typescript-win32-{arch}")).join("lib");
            std::fs::create_dir_all(&exe).unwrap();
            std::fs::write(exe.join("tsc.exe"), "").unwrap();
        }
        let local = |p: Option<(PathBuf, u64)>| p.is_some_and(|(d, _)| d.starts_with(&root));
        assert!(local(typescript_package(&root, true)), "trusted: the workspace's own TypeScript");
        assert!(!local(typescript_package(&root, false)), "untrusted: never the workspace's");
        for spec in default_specs() {
            if let Some(cmd) = spec.launch_command(&root, false) {
                assert!(!Path::new(&cmd[0]).starts_with(&root), "{}: {cmd:?}", spec.id);
            }
            let opts = spec.init_options_for(&root, false);
            let tsserver = opts.pointer("/tsserver/path").and_then(Value::as_str).unwrap_or("");
            assert!(!Path::new(tsserver).starts_with(&root), "{}: {opts}", spec.id);
        }
        if cfg!(windows) {
            assert!(native_ts_command(&root, true).is_some_and(|c| Path::new(&c[0]).starts_with(&root)), "trusted runs it");
        }
        let _ = std::fs::remove_dir_all(&root);

        // rust-analyzer: build scripts, proc macros and cargo check only when trusted.
        let ra = default_specs().into_iter().find(|s| s.id == "rust-analyzer").unwrap();
        let (t, u) = (ra.init_options_for(&root, true), ra.init_options_for(&root, false));
        let flags = |o: &Value| (o["cargo"]["buildScripts"]["enable"].clone(), o["procMacro"]["enable"].clone(), o["checkOnSave"].clone());
        assert_eq!(flags(&t), (json!(true), json!(true), json!(true)));
        assert_eq!(flags(&u), (json!(false), json!(false), json!(false)));
    }

    #[test]
    fn config_that_names_a_program_refuses_an_untrusted_start() {
        let root = std::env::temp_dir().join(format!("marlowe-hazard-{}", std::process::id()));
        std::fs::create_dir_all(root.join("crate-a/.cargo")).unwrap();
        let specs = default_specs();
        let ra = specs.iter().find(|s| s.id == "rust-analyzer").unwrap();
        let py = specs.iter().find(|s| s.id == "pyright").unwrap();
        assert_eq!(ra.untrusted_hazard(&root), None);
        assert_eq!(py.untrusted_hazard(&root), None);
        // One level down counts (rust-analyzer discovers projects there).
        std::fs::write(root.join("crate-a/.cargo/config.toml"), "[build]\nrustc-wrapper = \"evil.exe\"\n").unwrap();
        assert!(ra.untrusted_hazard(&root).is_some_and(|p| p.ends_with("config.toml")));
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(py.untrusted_hazard(&root), None);
        std::fs::write(root.join("pyproject.toml"), "[tool.pyright]\nvenvPath = \".\"\nvenv = \"evil\"\n").unwrap();
        assert!(py.untrusted_hazard(&root).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }
}
