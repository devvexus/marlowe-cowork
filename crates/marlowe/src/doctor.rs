//! `marlowe doctor` (U13): what this machine has and what is missing. Install, the daemon, which
//! API keys are present (never their values), language servers, `gh`, Docker, Ollama, shells and
//! the terminal. `--json` prints the same report as one object. Exit 0 unless the home directory
//! cannot be written (nothing else here is fatal; a missing tool is a line, not an error).

use std::path::{Path, PathBuf};
use std::time::Duration;

use marlowe_brains::{detect_ollama, KeyringStore, OLLAMA_ADDR};
use marlowe_model::ProviderKind;
use marlowe_os::child::{self, ChildSpec, RunLimits};
use serde_json::{json, Value};

use crate::daemon_ctl;
use crate::headless::driver::key_source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Present but not usable as is (not signed in, narrow terminal).
    Warn,
    Missing,
    /// Not needed for anything this build does.
    Info,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Missing => "MISS",
            Status::Info => "info",
        }
    }
    fn word(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Missing => "missing",
            Status::Info => "info",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

fn check(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    Check { name, status, detail: detail.into() }
}

/// What the report asks the machine; the tests answer instead.
pub trait Probe {
    fn which(&self, names: &[&str]) -> Option<PathBuf>;
    /// Runs a program (scrubbed, short timeout): `(exit code, stdout then stderr)`.
    fn run(&self, program: &Path, args: &[&str]) -> Option<(i32, String)>;
    fn ollama(&self) -> bool;
    /// `credential_manager`, `env` or `none` for the provider's key.
    fn key(&self, kind: ProviderKind) -> &'static str;
    fn terminal(&self) -> Option<(u16, u16)>;
    fn home_writable(&self, home: &Path) -> bool;
    /// The bash the `bash` tool runs (Git Bash on Windows, never the WSL launcher).
    fn bash(&self) -> Option<PathBuf> {
        self.which(&["bash.exe", "bash"])
    }
}

pub struct System;

impl Probe for System {
    fn which(&self, names: &[&str]) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
    }
    fn run(&self, program: &Path, args: &[&str]) -> Option<(i32, String)> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        let out = child::run(cmd, &ChildSpec::new(), RunLimits { timeout: Duration::from_secs(8), max_output_bytes: 16 * 1024 }, &|| false).ok()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Some((out.code(), text))
    }
    fn ollama(&self) -> bool {
        detect_ollama(OLLAMA_ADDR, Duration::from_millis(800))
    }
    fn key(&self, kind: ProviderKind) -> &'static str {
        key_source(kind, &KeyringStore::new(), &|k| std::env::var(k).ok())
    }
    fn terminal(&self) -> Option<(u16, u16)> {
        crossterm::terminal::size().ok()
    }
    fn home_writable(&self, home: &Path) -> bool {
        let f = home.join(format!(".doctor-{}", std::process::id()));
        let ok = std::fs::create_dir_all(home).is_ok() && std::fs::write(&f, b"x").is_ok();
        let _ = std::fs::remove_file(&f);
        ok
    }
    fn bash(&self) -> Option<PathBuf> {
        marlowe_os::bash::git_bash()
    }
}

fn find(p: &dyn Probe, base: &str) -> Option<PathBuf> {
    let names = [format!("{base}.exe"), format!("{base}.cmd"), format!("{base}.bat"), base.to_string()];
    p.which(&names.iter().map(String::as_str).collect::<Vec<_>>())
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

/// The whole report. `daemon` is the running daemon's port, if any (the caller looks it up
/// because that needs the home directory and a handshake).
pub fn report(p: &dyn Probe, home: &Path, daemon: Option<u16>) -> Vec<Check> {
    let mut out = Vec::new();

    let exe_path = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_else(|_| "unknown".into());
    out.push(check("install", Status::Ok, format!("marlowe {} at {exe_path}", env!("CARGO_PKG_VERSION"))));
    out.push(if p.home_writable(home) {
        check("home", Status::Ok, home.display().to_string())
    } else {
        check("home", Status::Missing, format!("{} is not writable", home.display()))
    });
    out.push(match daemon {
        Some(port) => check("daemon", Status::Ok, format!("running on port {port}")),
        None => check("daemon", Status::Info, "not running (starts on demand)"),
    });

    for (name, kind) in [("openrouter key", ProviderKind::OpenRouter), ("anthropic key", ProviderKind::Anthropic)] {
        out.push(match p.key(kind) {
            "none" => check(name, Status::Info, "none (environment variable or the Library)"),
            how => check(name, Status::Ok, format!("present ({})", how.replace('_', " "))),
        });
    }
    out.push(if p.ollama() { check("ollama", Status::Ok, OLLAMA_ADDR) } else { check("ollama", Status::Missing, "not reachable") });

    for (name, bin) in [("rust-analyzer", "rust-analyzer"), ("pyright", "pyright-langserver"), ("typescript-ls", "typescript-language-server")] {
        out.push(match find(p, bin) {
            Some(path) => check(name, Status::Ok, path.display().to_string()),
            None => check(name, Status::Info, "not found (code intelligence falls back to tags)"),
        });
    }

    out.push(match find(p, "git") {
        Some(path) => check("git", Status::Ok, p.run(&path, &["--version"]).map(|(_, t)| first_line(&t)).unwrap_or_else(|| path.display().to_string())),
        None => check("git", Status::Missing, "git not found; worktrees, checkpoints and diffs need it"),
    });
    out.push(match find(p, "gh") {
        None => check("gh", Status::Info, "not installed (pull requests and CI need it)"),
        Some(path) => match p.run(&path, &["auth", "status"]) {
            Some((0, _)) => check("gh", Status::Ok, "signed in"),
            Some((_, t)) => check("gh", Status::Warn, format!("not signed in: run `gh auth login` ({})", first_line(&t))),
            None => check("gh", Status::Warn, format!("{} did not answer", path.display())),
        },
    });
    out.push(match find(p, "docker") {
        Some(path) => check("docker", Status::Info, format!("{} (container workspaces are not supported in this build)", path.display())),
        None => check("docker", Status::Info, "not installed (not needed)"),
    });

    out.push(match p.bash() {
        Some(path) => check("bash", Status::Ok, path.display().to_string()),
        None => check("bash", Status::Missing, "Git Bash not found (the `bash` tool and hooks need it; install Git for Windows or set MARLOWE_BASH)"),
    });
    for (name, base) in [("powershell", "powershell"), ("pwsh", "pwsh"), ("python", "python"), ("node", "node")] {
        let found = find(p, base).or_else(|| if base == "python" { find(p, "python3") } else { None });
        out.push(match found {
            Some(path) => check(name, Status::Ok, path.display().to_string()),
            None => check(name, if matches!(base, "pwsh" | "node") { Status::Info } else { Status::Missing }, "not found"),
        });
    }
    out.push(match find(p, "wt") {
        Some(path) => check("windows terminal", Status::Ok, path.display().to_string()),
        None => check("windows terminal", Status::Info, "wt.exe not found"),
    });
    out.push(match p.terminal() {
        Some((w, h)) if w >= 120 && h >= 30 => check("terminal", Status::Ok, format!("{w}x{h}")),
        Some((w, h)) => check("terminal", Status::Warn, format!("{w}x{h} (the interface wants 120x30)")),
        None => check("terminal", Status::Info, "no terminal attached"),
    });
    out
}

/// The editor connection (U14) for `workspace`: which window would be connected, or why none.
pub fn editor_check(home: &Path, workspace: Option<&Path>) -> Check {
    let said = marlowe_mcp::ide::status(home, workspace);
    let status = if said.contains(" connected as ") { Status::Ok } else { Status::Info };
    check("editor", status, said)
}

pub fn render_text(home: &Path, checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        s.push_str(&format!("{} {:<17} {}\n", c.status.tag(), c.name, c.detail));
    }
    s.push_str(&format!("home {}\n", home.display()));
    s.push_str("braille \u{2801}\u{2803}\u{2807}\u{280f}\u{281f}\u{283f}\u{287f}\u{28ff}  (blanks mean the font lacks U+2800-28FF)\n");
    s
}

pub fn render_json(checks: &[Check]) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "checks": checks.iter().map(|c| json!({"name": c.name, "status": c.status.word(), "detail": c.detail})).collect::<Vec<_>>(),
    })
}

pub fn run(home: &Path, args: &[String]) -> i32 {
    let port = daemon_ctl::live(home).map(|i| i.port);
    let mut checks = report(&System, home, port);
    // Voice is off until MARLOWE_VOICE=1 (M6); the engine is only looked at when it is asked for.
    let voice = marlowe_voice::VoiceSettings::load(home).with_env(&|k| std::env::var(k).ok());
    checks.push(if marlowe_voice::requested(&|k| std::env::var(k).ok()) {
        match marlowe_voice::engine_check(&voice) {
            Ok(d) => check("voice", Status::Ok, d),
            Err(e) => check("voice", Status::Info, format!("{e}; WinRT speaks instead")),
        }
    } else {
        check("voice", Status::Info, format!("off (set {} to start it)", marlowe_voice::ENV_ENABLE))
    });
    checks.push(editor_check(home, std::env::current_dir().ok().as_deref()));
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&render_json(&checks)).unwrap_or_default());
    } else {
        print!("{}", render_text(home, &checks));
    }
    i32::from(checks.iter().any(|c| c.name == "home" && c.status == Status::Missing))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        on_path: Vec<&'static str>,
        keys: HashMap<&'static str, &'static str>,
        gh_signed_in: bool,
        ollama: bool,
        term: Option<(u16, u16)>,
        writable: bool,
    }

    impl Probe for Fake {
        fn which(&self, names: &[&str]) -> Option<PathBuf> {
            names.iter().find(|n| self.on_path.contains(n)).map(|n| PathBuf::from(format!("C:/fake/{n}")))
        }
        fn run(&self, program: &Path, args: &[&str]) -> Option<(i32, String)> {
            let name = program.file_name()?.to_string_lossy().to_string();
            match (name.as_str(), args) {
                ("git.exe", ["--version"]) => Some((0, "git version 2.50.0\n".into())),
                ("gh.exe", ["auth", "status"]) if self.gh_signed_in => Some((0, "Logged in\n".into())),
                ("gh.exe", ["auth", "status"]) => Some((1, "You are not logged into any GitHub hosts.\n".into())),
                _ => None,
            }
        }
        fn ollama(&self) -> bool {
            self.ollama
        }
        fn key(&self, kind: ProviderKind) -> &'static str {
            self.keys.get(kind.as_str()).copied().unwrap_or("none")
        }
        fn terminal(&self) -> Option<(u16, u16)> {
            self.term
        }
        fn home_writable(&self, _: &Path) -> bool {
            self.writable
        }
    }

    fn by_name<'a>(c: &'a [Check], n: &str) -> &'a Check {
        c.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("no {n} line in {c:?}"))
    }

    #[test]
    fn parity_u13_the_report_names_install_keys_lsps_gh_docker_and_ollama() {
        let f = Fake {
            on_path: vec!["git.exe", "gh.exe", "rust-analyzer.exe", "pyright-langserver.cmd", "bash.exe", "python.exe", "powershell.exe", "docker.exe"],
            keys: HashMap::from([("openrouter", "env")]),
            gh_signed_in: true,
            ollama: true,
            term: Some((140, 40)),
            writable: true,
        };
        let c = report(&f, Path::new("C:/home"), Some(4242));
        assert_eq!(by_name(&c, "install").status, Status::Ok);
        assert!(by_name(&c, "install").detail.starts_with("marlowe "));
        assert_eq!(by_name(&c, "daemon").detail, "running on port 4242");
        assert_eq!(by_name(&c, "openrouter key").detail, "present (env)");
        assert_eq!(by_name(&c, "anthropic key").status, Status::Info);
        assert_eq!(by_name(&c, "ollama").status, Status::Ok);
        assert_eq!(by_name(&c, "rust-analyzer").status, Status::Ok);
        assert_eq!(by_name(&c, "pyright").status, Status::Ok, "the .cmd shim npm installs counts");
        assert_eq!(by_name(&c, "typescript-ls").status, Status::Info);
        assert_eq!(by_name(&c, "git").detail, "git version 2.50.0");
        assert_eq!(by_name(&c, "gh").detail, "signed in");
        assert!(by_name(&c, "docker").detail.contains("not supported"));
        assert_eq!(by_name(&c, "terminal").detail, "140x40");
    }

    #[test]
    fn a_bare_machine_reports_what_is_missing_without_failing() {
        let c = report(&Fake { writable: true, ..Default::default() }, Path::new("C:/home"), None);
        assert_eq!(by_name(&c, "ollama").status, Status::Missing);
        assert_eq!(by_name(&c, "git").status, Status::Missing);
        assert_eq!(by_name(&c, "gh").status, Status::Info);
        assert_eq!(by_name(&c, "daemon").status, Status::Info);
        assert_eq!(by_name(&c, "terminal").status, Status::Info);
    }

    #[test]
    fn gh_without_a_login_says_how_to_fix_it_and_a_narrow_terminal_warns() {
        let f = Fake { on_path: vec!["gh.exe"], term: Some((80, 24)), writable: true, ..Default::default() };
        let c = report(&f, Path::new("C:/home"), None);
        assert_eq!(by_name(&c, "gh").status, Status::Warn);
        assert!(by_name(&c, "gh").detail.contains("gh auth login"));
        assert_eq!(by_name(&c, "terminal").status, Status::Warn);
    }

    #[test]
    fn a_key_source_reaches_the_report_in_text_and_json_and_nothing_else() {
        let f = Fake { keys: HashMap::from([("anthropic", "credential_manager")]), writable: true, ..Default::default() };
        let c = report(&f, Path::new("C:/home"), None);
        assert_eq!(by_name(&c, "anthropic key").detail, "present (credential manager)");
        let text = render_text(Path::new("C:/home"), &c);
        assert!(text.contains("ok   anthropic key") && text.contains("braille"), "{text}");
        let j = render_json(&c);
        assert!(j["checks"].as_array().unwrap().iter().any(|x| x["name"] == "anthropic key" && x["status"] == "ok"));
    }

    #[test]
    fn parity_u14_the_editor_line_says_connected_or_how_to_connect() {
        let home = std::env::temp_dir().join(format!("marlowe-doctor-ide-{}", std::process::id()));
        std::fs::create_dir_all(home.join("ide")).unwrap();
        let none = editor_check(&home, Some(Path::new("C:/work/proj")));
        assert_eq!(none.status, Status::Info);
        assert!(none.detail.contains("no editor running"), "{}", none.detail);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let lock = serde_json::json!({
            "version": 1, "pid": 1, "ideName": "Visual Studio Code", "transport": "http",
            "url": format!("http://127.0.0.1:{port}/mcp"), "authToken": "abc123",
            "workspaceFolders": ["C:/work/proj"], "started": "2026-10-11T10:00:00Z",
        });
        std::fs::write(home.join("ide").join(format!("{port}.lock")), lock.to_string()).unwrap();
        let on = editor_check(&home, Some(Path::new("C:/work/proj/src")));
        assert_eq!(on.status, Status::Ok);
        assert!(on.detail.contains("Visual Studio Code connected as `ide`"), "{}", on.detail);
        let other = editor_check(&home, Some(Path::new("D:/elsewhere")));
        assert_eq!(other.status, Status::Info);
        assert!(other.detail.contains("none has"), "{}", other.detail);
    }

    #[test]
    fn an_unwritable_home_is_the_one_failure() {
        let c = report(&Fake::default(), Path::new("C:/ro"), None);
        assert_eq!(by_name(&c, "home").status, Status::Missing);
    }
}
