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
//! this process (no sentinel, voice or crew; memory unless `--no-memory`). stdout carries only the
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

fn standalone(home: &Path, memory: bool, strict_mcp: bool) -> Result<(marlowe_proto::DaemonInfo, Box<dyn FnOnce()>), String> {
    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    if strict_mcp {
        // Read by the daemon's MCP host when it builds a workspace's hub.
        std::env::set_var(marlowe_mcp::STRICT_ENV, "1");
    }
    let deps = crate::wire::standalone_deps(home, memory);
    let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
    let daemon = Daemon::start(deps, home, cfg).map_err(|e| match e {
        DaemonError::AlreadyRunning(h) => format!(
            "--standalone: a daemon is already serving {h}; point MARLOWE_HOME at a directory of its own (or stop that daemon with `marlowe shutdown`)"
        ),
        other => other.to_string(),
    })?;
    let info = daemon.info();
    Ok((
        info,
        Box::new(move || {
            daemon.shutdown();
            let _ = daemon.wait();
        }),
    ))
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
