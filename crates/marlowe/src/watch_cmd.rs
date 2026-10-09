//! Headless Sentinel and voice commands: `marlowe watch`, `marlowe brief`, `marlowe say`.

use std::path::Path;
use std::time::Duration;

use marlowe_proto::{Event, Request, RunStatus, WatchCandidate, WatchState};

use crate::client::{Client, ClientError};
use crate::daemon_ctl;

const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Discovery walks up to four levels of the projects directory.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);

/// A brief may wait on the Sentinel's first scan and a 6 s polish.
const BRIEF_TIMEOUT: Duration = Duration::from_secs(60);

pub const WATCH_USAGE: &str = "usage: marlowe watch [list | add <dir> | remove <dir> | candidates]";

type CmdResult = Result<i32, String>;

fn connect(home: &Path, name: &str) -> Result<Client, String> {
    let info = daemon_ctl::ensure(home)?;
    Client::connect(&info, name).map_err(|e| e.to_string())
}

fn flatten<T>(r: Result<Result<T, String>, ClientError>) -> Result<T, String> {
    r.map_err(|e| e.to_string())?
}

fn send(c: &mut Client, req: &Request) -> Result<(), String> {
    c.send(req).map_err(|e| e.to_string())
}

fn watch_state(e: &Event) -> Option<WatchState> {
    match e {
        Event::WatchState(s) => Some(s.clone()),
        _ => None,
    }
}

/// Case- and separator-insensitive path equality (Windows paths).
fn same_path(a: &str, b: &str) -> bool {
    let norm = |p: &str| p.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

/// The first `WatchState` for which `pred` holds: a periodic state from before the change is
/// skipped.
fn state_where(c: &mut Client, pred: impl Fn(&WatchState) -> bool) -> Result<WatchState, String> {
    flatten(c.wait_for(OP_TIMEOUT, |e| watch_state(e).filter(|s| pred(s))))
}

pub fn watch(home: &Path, args: &[String]) -> CmdResult {
    let mut c = connect(home, "marlowe-watch")?;
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            send(&mut c, &Request::WatchList)?;
            print_state(&flatten(c.wait_for(OP_TIMEOUT, watch_state))?);
        }
        "add" => {
            let dir = absolute_dir(args.get(1).ok_or(WATCH_USAGE)?)?;
            send(&mut c, &Request::WatchAdd { path: dir.clone() })?;
            print_state(&state_where(&mut c, |s| s.roots.iter().any(|r| same_path(&r.path, &dir)))?);
            println!("watching {dir}");
        }
        "remove" => {
            let arg = args.get(1).ok_or(WATCH_USAGE)?;
            // A removed root may no longer exist; fall back to the text as given.
            let dir = absolute_dir(arg).unwrap_or_else(|_| arg.clone());
            send(&mut c, &Request::WatchRemove { path: dir.clone() })?;
            print_state(&state_where(&mut c, |s| !s.roots.iter().any(|r| same_path(&r.path, &dir)))?);
            println!("stopped watching {dir}");
        }
        "candidates" => {
            send(&mut c, &Request::WatchCandidates)?;
            let list = flatten(c.wait_for(DISCOVERY_TIMEOUT, |e| match e {
                Event::WatchCandidates { candidates } => Some(candidates.clone()),
                _ => None,
            }))?;
            print_candidates(&list);
        }
        other => return Err(format!("unknown watch command {other:?}\n{WATCH_USAGE}")),
    }
    Ok(0)
}

/// An existing directory as an absolute path without the `\\?\` prefix.
fn absolute_dir(arg: &str) -> Result<String, String> {
    let p = std::fs::canonicalize(arg).map_err(|e| format!("{arg}: {e}"))?;
    if !p.is_dir() {
        return Err(format!("{arg} is not a directory"));
    }
    let s = p.display().to_string();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.to_string(),
        _ => s,
    })
}

fn glyph(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Alive => "●",
        RunStatus::Stalled => "◐",
        RunStatus::Finished => "✓",
        RunStatus::Failed => "✗",
        RunStatus::Ended => "○",
    }
}

fn status_word(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Alive => "alive",
        RunStatus::Stalled => "stalled",
        RunStatus::Finished => "finished",
        RunStatus::Failed => "failed",
        RunStatus::Ended => "ended",
    }
}

fn print_state(s: &WatchState) {
    if s.roots.is_empty() {
        println!("no watch roots (marlowe watch candidates · marlowe watch add <dir>)");
    }
    for r in &s.roots {
        let mut line = format!("root {}  {} run{}", r.path, r.runs, if r.runs == 1 { "" } else { "s" });
        if let Some(g) = &r.git {
            line.push_str(&format!("  git {} · {} dirty", if g.branch.is_empty() { "-" } else { &g.branch }, g.dirty));
        }
        if let Some(p) = &r.problem {
            line.push_str(&format!("  ({p})"));
        }
        println!("{line}");
    }
    for r in &s.runs {
        let mut detail = Vec::new();
        if let Some(st) = &r.stage {
            detail.push(format!("stage {st}"));
        }
        if let Some(l) = r.layer {
            detail.push(format!("layer {l}"));
        }
        if let Some(st) = r.step {
            detail.push(format!("step {st}"));
        }
        if let Some(l) = r.loss {
            detail.push(format!("loss {l}"));
        }
        println!("{} {:<9} {}  {}", glyph(r.status), status_word(r.status), r.name, detail.join(" · "));
        if let Some(l) = &r.last_line {
            println!("            {}", l.chars().filter(|c| !c.is_control()).collect::<String>());
        }
    }
    match &s.gpu.unavailable {
        Some(why) => println!("gpu      unavailable ({why})"),
        None => {
            for d in &s.gpu.devices {
                println!(
                    "gpu{}     {} · {}% · {}/{} MB{}",
                    d.index,
                    d.name,
                    d.util_pct,
                    d.mem_used_mb,
                    d.mem_total_mb,
                    d.temp_c.map(|t| format!(" · {t} C")).unwrap_or_default()
                );
            }
        }
    }
    if s.ollama.reachable {
        let new = if s.ollama.new_models.is_empty() { String::new() } else { format!(" · new: {}", s.ollama.new_models.join(", ")) };
        println!("ollama   {} models{new}", s.ollama.models);
    } else {
        println!("ollama   not reachable");
    }
    println!("quiet    {}", s.quiet.label());
}

fn print_candidates(list: &[WatchCandidate]) {
    if list.is_empty() {
        println!("no candidates: nothing under the projects directory looks like a recent run");
        return;
    }
    for c in list {
        let live = if c.live_runs > 0 { format!(" · {} live", c.live_runs) } else { String::new() };
        println!("{}\n    {}{live}", c.path, c.reason);
    }
    println!("add one with: marlowe watch add <dir>");
}

pub fn brief(home: &Path) -> CmdResult {
    let mut c = connect(home, "marlowe-brief")?;
    send(&mut c, &Request::Brief)?;
    let (text, offers) = flatten(c.wait_for(BRIEF_TIMEOUT, |e| match e {
        Event::Brief { text, offers, .. } => Some((text.clone(), offers.clone())),
        _ => None,
    }))?;
    println!("{text}");
    for o in offers {
        println!("  marlowe watch add \"{}\"", o.path);
    }
    Ok(0)
}

pub fn say(home: &Path, args: &[String]) -> CmdResult {
    let text = args.join(" ");
    if text.trim().is_empty() {
        return Err("usage: marlowe say <text>".into());
    }
    let mut c = connect(home, "marlowe-say")?;
    send(&mut c, &Request::Speak { text })?;
    // Success has no reply of its own: the voice reports through VoiceState once it speaks.
    match c.wait_for(OP_TIMEOUT, |e| match e {
        Event::VoiceState(v) if v.speaking => Some(v.voice.clone()),
        _ => None,
    }) {
        Ok(Ok(voice)) => {
            println!("speaking{}", voice.map(|v| format!(" · {v}")).unwrap_or_default());
            Ok(0)
        }
        Ok(Err(message)) => Err(message),
        Err(ClientError::Timeout) => Err("the voice did not start within 30 s".into()),
        Err(e) => Err(e.to_string()),
    }
}
