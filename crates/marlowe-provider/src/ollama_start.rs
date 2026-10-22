//! Starting Ollama when it is not answering, with the environment the user has saved now.
//!
//! A terminal, and so this process, carries the environment it was opened with; Ollama must read
//! the one in the registry (`HKCU\Environment` over the system's), so `OLLAMA_NUM_PARALLEL` set
//! after the terminal opened still applies. The installed app is preferred, else `ollama serve`;
//! either starts hidden and detached so closing Marlowe never takes it along. A running Ollama is
//! never touched here: restarting one is [`OllamaServerControl::apply`](crate::OllamaServerControl),
//! behind an approval. Everything outside the decision (the probe, the process start, the
//! registry, the clock) is behind [`StartIo`], so the logic is tested with fakes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marlowe_hw::EngineStart;

use crate::http::{self, LocalEndpoint};

/// How long a freshly started Ollama has to answer.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(400);
const PROBE: Duration = Duration::from_millis(2500);

/// What starting needs from the machine.
pub trait StartIo {
    /// Whether the server answers `/api/version`.
    fn answers(&self) -> bool;
    /// The installed Ollama tray app, if present.
    fn app(&self) -> Option<PathBuf>;
    /// The `ollama` command line tool, if found.
    fn cli(&self) -> Option<PathBuf>;
    /// The environment to start it with: this process's, overlaid with the saved one, scrubbed.
    fn environment(&self) -> Vec<(String, String)>;
    /// Starts `program args` hidden and detached with exactly `env` as its environment.
    fn spawn(&self, program: &Path, args: &[&str], env: &[(String, String)]) -> std::io::Result<()>;
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
}

/// Starts Ollama if it is not answering. `progress` hears "starting Ollama" once, when it begins.
pub fn ensure_running(io: &dyn StartIo, endpoint: &LocalEndpoint, timeout: Duration, progress: &mut dyn FnMut(&str)) -> EngineStart {
    if io.answers() {
        return EngineStart::Running;
    }
    let (program, args): (PathBuf, &[&str]) = match (io.app(), io.cli()) {
        (Some(a), _) => (a, &[]),
        (None, Some(c)) => (c, &["serve"]),
        (None, None) => {
            return EngineStart::Failed(format!(
                "Ollama is not running at {endpoint} and is not installed where Marlowe looks (the Ollama app, or `ollama` on PATH). Install it from ollama.com."
            ))
        }
    };
    let env = io.environment();
    progress("starting Ollama");
    if let Err(e) = io.spawn(&program, args, &env) {
        return EngineStart::Failed(format!("Ollama is not running at {endpoint} and {} would not start: {e}", program.display()));
    }
    let began = io.now();
    loop {
        let waited = io.now().saturating_duration_since(began);
        if io.answers() {
            return EngineStart::Started(format!("Ollama started in {:.1} s", waited.as_secs_f64()));
        }
        if waited >= timeout {
            return EngineStart::Failed(format!(
                "started {} but Ollama did not answer at {endpoint} within {} s. Check %LOCALAPPDATA%\\Ollama\\server.log.",
                program.display(),
                timeout.as_secs()
            ));
        }
        io.sleep(POLL);
    }
}

/// `reg query` output as (name, value) pairs: `    NAME    REG_SZ    value`.
pub fn parse_reg_all(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter(|l| l.starts_with(' ') || l.starts_with('\t'))
        .filter_map(|l| {
            let mut it = l.trim().splitn(3, "    ");
            let name = it.next()?.trim();
            let ty = it.next()?.trim();
            let value = it.next().unwrap_or("").trim();
            (ty.starts_with("REG_") && !name.is_empty()).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

/// `%NAME%` references expanded from `lookup` (unknown ones stay as written).
pub fn expand_vars(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        match tail.find('%') {
            Some(j) if j > 0 => match lookup(&tail[..j]) {
                Some(v) => {
                    out.push_str(&v);
                    rest = &tail[j + 1..];
                }
                None => {
                    out.push('%');
                    rest = tail;
                }
            },
            _ => {
                out.push('%');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// This process's environment with the saved system then user environment laid over it, minus the
/// `scrub` names. `PATH` is the system's followed by the user's (as Windows builds it).
pub fn overlay_environment(
    process: Vec<(String, String)>,
    system: &[(String, String)],
    user: &[(String, String)],
    scrub: &[String],
) -> Vec<(String, String)> {
    fn find(env: &[(String, String)], k: &str) -> Option<usize> {
        env.iter().position(|(n, _)| n.eq_ignore_ascii_case(k))
    }
    fn set(env: &mut Vec<(String, String)>, k: &str, v: String) {
        match find(env, k) {
            Some(i) => env[i].1 = v,
            None => env.push((k.to_string(), v)),
        }
    }
    let mut env = process;
    for (k, v) in system.iter().chain(user.iter()) {
        if k.eq_ignore_ascii_case("PATH") {
            continue;
        }
        let v = expand_vars(v, &|n| find(&env, n).map(|i| env[i].1.clone()));
        set(&mut env, k, v);
    }
    let path_of = |list: &[(String, String)], env: &[(String, String)]| {
        list.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| expand_vars(v, &|n| find(env, n).map(|i| env[i].1.clone())))
    };
    let joined: Vec<String> = [path_of(system, &env), path_of(user, &env)].into_iter().flatten().collect();
    if !joined.is_empty() {
        set(&mut env, "Path", joined.join(";"));
    }
    env.retain(|(k, _)| !scrub.iter().any(|s| s.eq_ignore_ascii_case(k)));
    env
}

/// The real machine.
pub struct SystemIo {
    pub endpoint: LocalEndpoint,
    pub scrub: Vec<String>,
}

impl StartIo for SystemIo {
    fn answers(&self) -> bool {
        // Twice: an Ollama busy with a load can be slow to answer, and that is not "stopped".
        (0..2).any(|_| http::get_json(&self.endpoint, "/api/version", PROBE).is_ok())
    }

    fn app(&self) -> Option<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")?;
        let p = PathBuf::from(local).join("Programs").join("Ollama").join("ollama app.exe");
        p.is_file().then_some(p)
    }

    fn cli(&self) -> Option<PathBuf> {
        let name = if cfg!(windows) { "ollama.exe" } else { "ollama" };
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("Programs").join("Ollama").join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
    }

    fn environment(&self) -> Vec<(String, String)> {
        let (system, user) = saved_environment();
        overlay_environment(std::env::vars().collect(), &system, &user, &self.scrub)
    }

    fn spawn(&self, program: &Path, args: &[&str], env: &[(String, String)]) -> std::io::Result<()> {
        let mut cmd = Command::new(program);
        cmd.args(args).env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
            cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
            if cmd.spawn().is_ok() {
                return Ok(());
            }
            // A job without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses the flag.
            cmd.creation_flags(base);
        }
        cmd.spawn().map(|_| ())
    }
}

/// The saved (system, user) environment from the registry; empty off Windows.
fn saved_environment() -> (Vec<(String, String)>, Vec<(String, String)>) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let query = |key: &str| -> Vec<(String, String)> {
            Command::new("reg")
                .args(["query", key])
                .creation_flags(0x0800_0000)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| parse_reg_all(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_default()
        };
        (query(r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"), query(r"HKCU\Environment"))
    }
    #[cfg(not(windows))]
    {
        (Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    type Spawn = (PathBuf, Vec<String>, Vec<(String, String)>);

    /// A server that answers on the `answers_after`-th probe following the spawn, on a clock that
    /// only moves when slept on.
    struct Fake {
        up: Cell<bool>,
        answers_after: Option<u32>,
        probes_since_spawn: Cell<u32>,
        spawned: RefCell<Vec<Spawn>>,
        app: Option<PathBuf>,
        cli: Option<PathBuf>,
        spawn_fails: bool,
        t0: Instant,
        slept: Cell<Duration>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                up: Cell::new(false),
                answers_after: Some(2),
                probes_since_spawn: Cell::new(0),
                spawned: RefCell::new(Vec::new()),
                app: Some(PathBuf::from("ollama app.exe")),
                cli: Some(PathBuf::from("ollama.exe")),
                spawn_fails: false,
                t0: Instant::now(),
                slept: Cell::new(Duration::ZERO),
            }
        }
    }

    impl StartIo for Fake {
        fn answers(&self) -> bool {
            if self.up.get() {
                return true;
            }
            if self.spawned.borrow().is_empty() {
                return false;
            }
            let n = self.probes_since_spawn.get() + 1;
            self.probes_since_spawn.set(n);
            matches!(self.answers_after, Some(k) if n >= k)
        }
        fn app(&self) -> Option<PathBuf> {
            self.app.clone()
        }
        fn cli(&self) -> Option<PathBuf> {
            self.cli.clone()
        }
        fn environment(&self) -> Vec<(String, String)> {
            vec![("OLLAMA_NUM_PARALLEL".into(), "6".into())]
        }
        fn spawn(&self, program: &Path, args: &[&str], env: &[(String, String)]) -> std::io::Result<()> {
            if self.spawn_fails {
                return Err(std::io::Error::other("access denied"));
            }
            self.spawned.borrow_mut().push((program.to_path_buf(), args.iter().map(|a| a.to_string()).collect(), env.to_vec()));
            Ok(())
        }
        fn now(&self) -> Instant {
            self.t0 + self.slept.get()
        }
        fn sleep(&self, d: Duration) {
            self.slept.set(self.slept.get() + d);
        }
    }

    fn ep() -> LocalEndpoint {
        LocalEndpoint::default_ollama()
    }

    #[test]
    fn a_running_ollama_is_left_alone() {
        let io = Fake::new();
        io.up.set(true);
        let mut said = Vec::new();
        let r = ensure_running(&io, &ep(), START_TIMEOUT, &mut |s| said.push(s.to_string()));
        assert_eq!(r, EngineStart::Running);
        assert!(io.spawned.borrow().is_empty() && said.is_empty(), "nothing started, nothing said");
    }

    #[test]
    fn the_app_is_started_with_the_saved_environment_and_waited_for() {
        let io = Fake::new();
        let mut said = Vec::new();
        let r = ensure_running(&io, &ep(), START_TIMEOUT, &mut |s| said.push(s.to_string()));
        assert!(matches!(r, EngineStart::Started(ref s) if s.contains("Ollama started")), "{r:?}");
        assert_eq!(said, ["starting Ollama"]);
        let spawned = io.spawned.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].0, PathBuf::from("ollama app.exe"));
        assert!(spawned[0].1.is_empty());
        assert!(spawned[0].2.contains(&("OLLAMA_NUM_PARALLEL".into(), "6".into())), "the saved environment reaches it");
    }

    #[test]
    fn without_the_app_it_is_ollama_serve() {
        let mut io = Fake::new();
        io.app = None;
        ensure_running(&io, &ep(), START_TIMEOUT, &mut |_| {});
        let spawned = io.spawned.borrow();
        assert_eq!(spawned[0].0, PathBuf::from("ollama.exe"));
        assert_eq!(spawned[0].1, ["serve"]);
    }

    #[test]
    fn not_installed_says_so_without_starting_anything() {
        let mut io = Fake::new();
        io.app = None;
        io.cli = None;
        let mut said = Vec::new();
        let r = ensure_running(&io, &ep(), START_TIMEOUT, &mut |s| said.push(s.to_string()));
        assert!(matches!(&r, EngineStart::Failed(w) if w.contains("not installed")), "{r:?}");
        assert!(said.is_empty(), "no start line for a start that never began");
    }

    #[test]
    fn a_start_that_fails_or_never_answers_says_why() {
        let mut io = Fake::new();
        io.spawn_fails = true;
        let r = ensure_running(&io, &ep(), START_TIMEOUT, &mut |_| {});
        assert!(matches!(&r, EngineStart::Failed(w) if w.contains("access denied")), "{r:?}");

        let mut io = Fake::new();
        io.answers_after = None;
        let r = ensure_running(&io, &ep(), Duration::from_secs(3), &mut |_| {});
        assert!(matches!(&r, EngineStart::Failed(w) if w.contains("did not answer") && w.contains("within 3 s")), "{r:?}");
        assert!(io.slept.get() >= Duration::from_secs(3), "it waited the whole time on the fake clock");
    }

    #[test]
    fn registry_output_is_parsed_and_overlaid_user_over_system() {
        let out = "\r\nHKEY_CURRENT_USER\\Environment\r\n    OLLAMA_NUM_PARALLEL    REG_SZ    6\r\n    Path    REG_EXPAND_SZ    %USERPROFILE%\\bin;C:\\tools\r\n    TEMP    REG_EXPAND_SZ    %USERPROFILE%\\AppData\\Local\\Temp\r\n\r\n";
        let user = parse_reg_all(out);
        assert_eq!(user[0], ("OLLAMA_NUM_PARALLEL".into(), "6".into()));
        let system = vec![("OLLAMA_NUM_PARALLEL".to_string(), "1".to_string()), ("Path".into(), "C:\\Windows".into())];
        let process = vec![
            ("OLLAMA_NUM_PARALLEL".to_string(), "2".to_string()),
            ("USERPROFILE".into(), "C:\\Users\\m".into()),
            ("OPENROUTER_API_KEY".into(), "secret".into()),
        ];
        let env = overlay_environment(process, &system, &user, &["OPENROUTER_API_KEY".to_string()]);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("OLLAMA_NUM_PARALLEL"), Some("6"), "the user's saved value beats the terminal's and the system's");
        assert_eq!(get("Path"), Some("C:\\Windows;C:\\Users\\m\\bin;C:\\tools"), "system then user, expanded");
        assert_eq!(get("TEMP"), Some("C:\\Users\\m\\AppData\\Local\\Temp"));
        assert_eq!(get("OPENROUTER_API_KEY"), None, "credentials never reach Ollama");
    }
}
