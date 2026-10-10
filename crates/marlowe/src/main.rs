//! The Marlowe binary: TUI client, daemon, launcher and headless commands.

// One audited exception: `daemon_ctl::stop_std_handle_inheritance` (Windows).
#![deny(unsafe_code)]

mod client;
mod commands;
mod crew_cmd;
mod daemon_ctl;
mod doctor;
mod eval;
mod launcher;
mod math_cmd;
mod me_cmd;
mod memory_cmd;
#[cfg(debug_assertions)]
mod scripted;
mod watch_cmd;
mod wire;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
marlowe                                   open the TUI (starts the daemon if needed)
marlowe --launch [args]                   open it in a configured Windows Terminal window
marlowe daemon                            run the daemon in the foreground
marlowe ask <text> [--model REF] [--workspace DIR] [--session ID] [--yes]
marlowe models [list | detect | catalog <provider> | add <provider> <id>
                | remove <ref> | slot <slot> <ref|default> [effort]]
marlowe sessions [limit]                  recent sessions: id, turns, workspace, title
marlowe memory [search <query> [--limit N] [--project DIR | --global] | stats
                | probe <query> [--workspace DIR] | forget <id>
                | correct <id> <text> | import [--from DIR] | bench [--n N]]
marlowe watch [list | add <dir> | remove <dir> | candidates]
                                          what the Sentinel watches; roots are added only here
                                          or in the TUI (candidates are offers, never added)
marlowe brief                             the brief now: what changed since you were last here
marlowe say <text>                        speak through the daemon's voice
marlowe crew [list [N] | show <run> | roles | dispatch <role> <goal> [--playbook P]
              [--workspace DIR] [--session ID] | steer <run> <text> | cancel <run>
              | pause <run> | resume <run> | merge <run> [--yes] | discard <run> [--yes]]
                                          crew runs; merge and discard ask for approval
marlowe me [--name N] [--address sir|name|none]
                                          what Marlowe calls you (prints it with no flags)
marlowe math [--grid | --images] [--width N]
                                          a sheet of hard equations and matrices, drawn as the
                                          conversation pane draws them (grid, and Sixel images)
marlowe status | shutdown | doctor

ask streams the reply to stdout. A tool that needs approval prompts on stdin; --yes allows
every approval in this turn; an empty answer or a closed stdin denies.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = marlowe_proto::paths::cowork_home();
    let result = dispatch(&home, &args);
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code.clamp(1, 255) as u8),
        Err(msg) => {
            eprintln!("marlowe: {msg}");
            ExitCode::from(2)
        }
    }
}

fn dispatch(home: &std::path::Path, args: &[String]) -> Result<i32, String> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        None | Some("tui") => run_tui(home),
        Some("--launch") => launch(rest),
        Some("daemon") => run_daemon(home),
        Some("ask") => ask(home, rest),
        Some("models") => commands::models(home, rest),
        Some("sessions") => commands::sessions(home, rest),
        Some("memory") => memory_cmd::run(home, rest),
        Some("watch") => watch_cmd::watch(home, rest),
        Some("brief") => watch_cmd::brief(home),
        Some("say") => watch_cmd::say(home, rest),
        Some("me") => me_cmd::run(home, rest),
        Some("crew") => crew_cmd::run(home, rest),
        Some("status") => commands::status(home),
        Some("shutdown") => commands::shutdown(home),
        Some("doctor") => Ok(doctor::run(home)),
        Some("math") => math_cmd::run(rest),
        Some("--eval-adapter") => eval::run(rest),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(0)
        }
        Some(other) => Err(format!("unknown command {other:?}\n{USAGE}")),
    }
}

fn run_tui(home: &std::path::Path) -> Result<i32, String> {
    let info = daemon_ctl::ensure(home)?;
    marlowe_tui::run(marlowe_tui::ConnectInfo::from(&info)).map_err(|e| e.to_string())?;
    Ok(0)
}

fn run_daemon(home: &std::path::Path) -> Result<i32, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    marlowe_daemon::run(wire::daemon_deps(home), home).map_err(|e| e.to_string())?;
    Ok(0)
}

fn launch(extra: &[String]) -> Result<i32, String> {
    let outcome = launcher::launch(extra).map_err(|e| e.to_string())?;
    for g in &outcome.globals_changed {
        eprintln!("changed Windows Terminal setting: {g}");
    }
    for d in &outcome.degraded {
        eprintln!("{d}");
    }
    if outcome.terminal.is_none() {
        // No terminal of ours: run in this one.
        let home = marlowe_proto::paths::cowork_home();
        return run_tui(&home);
    }
    Ok(0)
}

fn ask(home: &std::path::Path, rest: &[String]) -> Result<i32, String> {
    let mut text = Vec::new();
    let (mut model, mut workspace, mut session, mut yes) = (None, None, None, false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or(format!("{flag} needs a value"));
        match a.as_str() {
            "--model" => model = Some(value("--model")?),
            "--workspace" => workspace = Some(value("--workspace")?),
            "--session" => session = Some(value("--session")?),
            "--yes" => yes = true,
            _ => text.push(a.clone()),
        }
    }
    if text.is_empty() {
        return Err("usage: marlowe ask <text> [--model REF] [--workspace DIR] [--session ID] [--yes]".into());
    }
    // A new session defaults to the current directory unless that is too broad to be a workspace
    // (home, a drive root, a system directory); an existing session keeps its own.
    let workspace = workspace
        .map(PathBuf::from)
        .or_else(|| {
            session
                .is_none()
                .then(|| std::env::current_dir().ok())
                .flatten()
                .filter(|d| !marlowe_exec::paths::is_broad_root(d))
        })
        .map(|w| std::fs::canonicalize(&w).map(strip_verbatim).unwrap_or(w).display().to_string());
    commands::ask(home, commands::AskArgs { text: text.join(" "), model, workspace, session, yes })
}

/// `\\?\C:\x` -> `C:\x`; the verbatim prefix confuses downstream path scoping.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_verbatim;
    use std::path::PathBuf;

    #[test]
    fn verbatim_prefixes_are_stripped_except_unc() {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\work")), PathBuf::from(r"C:\work"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\?\UNC\srv\share"));
        assert_eq!(strip_verbatim(PathBuf::from(r"C:\work")), PathBuf::from(r"C:\work"));
    }
}
