//! `marlowe -p`: one non-interactive run, like `claude -p` (design 2.7 H1-H6).
//!
//! | Module | What |
//! |---|---|
//! | [`args`] | the flags, in Marlowe's and Claude Code's spellings |
//! | [`driver`] | connects, opens or resumes a session, runs turns, answers approvals, exits |
//! | [`input`], [`output`] | stream-json in and out (SDK shapes) |
//! | [`policy`] | `--allowed-tools` / `--disallowed-tools` rules |
//! | [`prompt_tool`] | `--permission-prompt-tool mcp__server__tool` |
//! | [`settings`], [`worktree`] | `--settings` / `--mcp-config` values, `-w` |
//!
//! The run talks to the shared daemon like any client, or with `--standalone` to a daemon inside
//! this process (no sentinel, voice or crew; memory unless `--no-memory`). When the home already has
//! a daemon, a standalone run cannot claim it, so it runs beside it on a private copy of the
//! model library, the user file and the MCP files, with no memory and no earlier sessions. stdout carries only the
//! answer; diagnostics go to stderr. Exit codes: 0 done, 1 error, 2 approval needed under
//! `--fail-on-ask`, 3 budget, 130 cancelled.

pub mod args;
pub mod driver;
pub mod input;
pub mod output;
pub mod policy;
pub mod prompt_tool;
pub mod settings;
pub mod worktree;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use marlowe_daemon::{Daemon, DaemonConfig, DaemonError};

use args::Args;
use driver::{Connector, Env, EXIT_ERROR};

const SUBCOMMANDS: &[&str] = &[
    "tui", "daemon", "ask", "models", "sessions", "memory", "watch", "brief", "say", "me", "crew", "status", "shutdown", "doctor", "bench", "export", "mcp",
    "eval", "math", "help",
];

/// Whether this command line is a print run: `-p` or `--print`, and no subcommand in front.
pub fn is_print(args: &[String]) -> bool {
    let first = args.first().map(String::as_str).unwrap_or("");
    !SUBCOMMANDS.contains(&first) && !matches!(first, "--launch" | "-h" | "--help" | "--eval-adapter") && args.iter().any(|a| a == "-p" || a == "--print")
}

/// The entry point `main.rs` dispatches to.
pub fn run(home: &Path, raw: &[String]) -> Result<i32, String> {
    if raw.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", args::USAGE);
        return Ok(0);
    }
    // A usage error is exit 1: exit 2 means "approval needed" here.
    let args = match Args::parse(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("marlowe: {e}\n\n{}", args::USAGE);
            return Ok(EXIT_ERROR);
        }
    };
    let interrupt = Arc::new(AtomicBool::new(false));
    install_ctrl_c(interrupt.clone());
    let stdin = std::io::stdin();
    let env = Env {
        home: home.to_path_buf(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        stdin: (!stdin.is_terminal()).then(|| Box::new(std::io::BufReader::new(stdin)) as Box<dyn std::io::BufRead + Send>),
        out: Box::new(std::io::stdout()),
        err: Box::new(std::io::stderr()),
        interrupt,
        daemon_applies_options: driver::DAEMON_APPLIES_OPTIONS,
        stdin_grace: driver::STDIN_GRACE,
    };
    let connect = connector(home, &args);
    Ok(driver::run(args, env, connect))
}

/// The shared daemon (started on demand), or one inside this process.
pub fn connector(home: &Path, args: &Args) -> Connector {
    let home = home.to_path_buf();
    if args.standalone {
        let (memory, strict) = (!args.no_memory, args.strict_mcp);
        Box::new(move || standalone(&home, memory, strict))
    } else {
        Box::new(move || Ok((crate::daemon_ctl::ensure(&home)?, Box::new(|| {}) as Box<dyn FnOnce()>)))
    }
}

type Started = (marlowe_proto::DaemonInfo, Box<dyn FnOnce()>);

fn standalone(home: &Path, memory: bool, strict_mcp: bool) -> Result<Started, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    if strict_mcp {
        // Read by the daemon's MCP host when it builds a workspace's hub.
        std::env::set_var(marlowe_mcp::STRICT_ENV, "1");
    }
    let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
    if serving(home) {
        return beside(home, cfg);
    }
    let deps = crate::wire::standalone_deps(home, memory);
    match Daemon::start(deps, home, cfg.clone()) {
        Ok(d) => Ok(running(d, None)),
        // Lost a race with a daemon starting.
        Err(DaemonError::AlreadyRunning(_)) => beside(home, cfg),
        Err(e) => Err(e.to_string()),
    }
}

fn running(daemon: Daemon, scratch: Option<PathBuf>) -> Started {
    let info = daemon.info();
    (
        info,
        Box::new(move || {
            daemon.shutdown();
            let _ = daemon.wait();
            if let Some(dir) = scratch {
                let _ = std::fs::remove_dir_all(dir);
            }
        }),
    )
}

/// Whether a daemon answers for `home` right now.
fn serving(home: &Path) -> bool {
    let Ok(Some(info)) = marlowe_proto::DaemonInfo::read(home) else { return false };
    std::net::TcpStream::connect_timeout(&info.addr(), std::time::Duration::from_millis(500)).is_ok()
}

/// Files of a profile home a private copy carries over.
const COPIED: &[&str] = &["user.json", "settings.toml", "settings.json", "research.json", "mcp.json", "mcp-trust.json", "mcp-pins.json"];

/// A daemon of this process's own, on a scratch home that copies what a run needs from `home`:
/// the model library (models, slot assignments, context policies), the user file, the settings and
/// the MCP files. Not the memory (the shared daemon owns the journal; two writers would fork its
/// chain) and not the sessions, so `-c` and `-r` find nothing here. Deleted when the run ends.
fn beside(home: &Path, cfg: DaemonConfig) -> Result<Started, String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let scratch = std::env::temp_dir().join(format!("marlowe-print-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&scratch).map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;
    let prepared = copy_profile(home, &scratch);
    if let Err(e) = prepared {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }
    eprintln!(
        "marlowe: a daemon already serves {}; --standalone runs beside it on a private copy of its model library, with no memory and no earlier sessions",
        home.display()
    );
    let deps = crate::wire::standalone_deps(&scratch, false);
    match Daemon::start(deps, &scratch, cfg) {
        Ok(d) => Ok(running(d, Some(scratch))),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            Err(e.to_string())
        }
    }
}

fn copy_profile(from: &Path, to: &Path) -> Result<(), String> {
    use marlowe_brains::Library;
    let src = Library::open(&marlowe_proto::paths::cowork_db(from)).map_err(|e| format!("cannot read the model library of {}: {e}", from.display()))?;
    let dst = Library::open(&marlowe_proto::paths::cowork_db(to)).map_err(|e| format!("cannot create a model library: {e}"))?;
    let fail = |e: marlowe_brains::BrainsError| format!("cannot copy the model library: {e}");
    for m in src.models().map_err(fail)? {
        dst.add(&m.info).map_err(fail)?;
    }
    for s in src.slots().map_err(fail)?.into_iter().filter(|s| !s.inherited) {
        dst.set_slot(&s.slot, s.model.as_ref(), s.effort).map_err(fail)?;
    }
    for (model, policy) in src.policies().map_err(fail)? {
        dst.set_policy(&model, &policy).map_err(fail)?;
    }
    for name in COPIED {
        let from_file = from.join(name);
        if from_file.is_file() {
            std::fs::copy(&from_file, to.join(name)).map_err(|e| format!("cannot copy {}: {e}", from_file.display()))?;
        }
    }
    Ok(())
}

/// Ctrl-C asks the run to cancel its turn and exit 130 instead of killing it mid-write.
#[cfg(windows)]
#[allow(unsafe_code)]
fn install_ctrl_c(flag: Arc<AtomicBool>) {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};

    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

    unsafe extern "system" fn handler(kind: u32) -> i32 {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            if let Some(f) = FLAG.get() {
                f.store(true, Ordering::Relaxed);
            }
            1
        } else {
            0
        }
    }

    if FLAG.set(flag).is_ok() {
        // SAFETY: `handler` is a plain function that only stores to a static atomic; it stays
        // valid for the process lifetime. The call registers it and touches nothing else.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}

#[cfg(not(windows))]
fn install_ctrl_c(_flag: Arc<AtomicBool>) {}

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_flags;
