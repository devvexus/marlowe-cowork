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

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::index::Lang;

/// Root-dependent `initializationOptions`.
pub type OptionsFn = fn(&Path, &Value) -> Value;
/// Root-dependent command (`None`: cannot run there).
pub type CommandFn = fn(&Path) -> Option<Vec<String>>;

/// One language server.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// Stable id (`rust-analyzer`, `pyright`, `typescript-language-server`, or a test's).
    pub id: &'static str,
    pub langs: Vec<Lang>,
    /// Program name or path, then arguments.
    pub command: Vec<String>,
    /// `initializationOptions`.
    pub init_options: Value,
    /// Extra environment for the child (after the credential scrub).
    pub env: Vec<(&'static str, &'static str)>,
    /// Files whose presence at the workspace root says this server is wanted there, so an edit
    /// starts it (the `diagnostics` tool and the symbol tools start it regardless).
    pub markers: Vec<&'static str>,
    /// The server reports `experimental/serverStatus` (rust-analyzer): requests wait for it to
    /// be quiescent after start.
    pub server_status: bool,
    /// `initializationOptions` for a given root, when they depend on it.
    pub options_for_root: Option<OptionsFn>,
    /// The command for a given root, when it depends on it (`None`: cannot run there).
    pub command_for_root: Option<CommandFn>,
}

impl ServerSpec {
    pub fn serves(&self, lang: Lang) -> bool {
        self.langs.contains(&lang)
    }

    /// The command to run for `root` with its program resolved; `None` when it cannot run.
    pub fn launch_command(&self, root: &Path) -> Option<Vec<String>> {
        if let Some(f) = self.command_for_root {
            return f(root);
        }
        let program = find_program(self.command.first()?)?;
        Some(std::iter::once(program.to_string_lossy().into_owned()).chain(self.command[1..].iter().cloned()).collect())
    }

    /// The `initializationOptions` to send for `root`.
    pub fn init_options_for(&self, root: &Path) -> Value {
        match self.options_for_root {
            Some(f) => f(root, &self.init_options),
            None => self.init_options.clone(),
        }
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
            options_for_root: None,
            command_for_root: None,
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
        },
    ]
}

/// The TypeScript package a root uses: its own `node_modules/typescript`, else the global npm
/// one beside `tsc`. With its major version.
fn typescript_package(root: &Path) -> Option<(PathBuf, u64)> {
    let local = root.join("node_modules").join("typescript");
    let global = find_program("tsc").and_then(|tsc| tsc.parent().map(|d| d.join("node_modules").join("typescript")));
    for dir in std::iter::once(local).chain(global) {
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { continue };
        let Ok(pkg) = serde_json::from_str::<Value>(&text) else { continue };
        let major = pkg.get("version").and_then(Value::as_str).and_then(|v| v.split('.').next()?.parse().ok()).unwrap_or(0);
        return Some((dir, major));
    }
    None
}

/// `tsserver.js` of the root's TypeScript (before 7, which has no tsserver).
fn tsserver_js(root: &Path) -> Option<PathBuf> {
    let (dir, _) = typescript_package(root)?;
    Some(dir.join("lib").join("tsserver.js")).filter(|p| p.is_file())
}

fn tls_command(root: &Path) -> Option<Vec<String>> {
    tsserver_js(root)?;
    let program = find_program("typescript-language-server")?;
    Some(vec![program.to_string_lossy().into_owned(), "--stdio".into()])
}

/// typescript-language-server is told which tsserver to use.
fn tsserver_options(root: &Path, base: &Value) -> Value {
    let mut opts = base.clone();
    if let Some(js) = tsserver_js(root) {
        opts["tsserver"] = json!({"path": js.to_string_lossy()});
    }
    opts
}

/// TypeScript 7's platform executable (`@typescript/typescript-<platform>-<arch>/lib/tsc.exe`).
fn native_ts_command(root: &Path) -> Option<Vec<String>> {
    let (dir, major) = typescript_package(root)?;
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
        assert_eq!(typescript_package(&root).map(|p| p.1), Some(5));
        assert_eq!(native_ts_command(&root), None);
        assert_eq!(tsserver_options(&root, &json!({}))["tsserver"]["path"], json!(lib.join("tsserver.js").to_string_lossy()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
