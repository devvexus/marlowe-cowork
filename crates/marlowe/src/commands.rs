//! Headless commands: ask, models, status, shutdown.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use marlowe_model::{Effort, ModelRef, ProviderKind};
use marlowe_proto::{
    ApprovalDecision, Event, LibraryModel, ProviderStatus, Request, SessionId, Slot, SlotAssignment,
    TurnStop,
};

use crate::client::{Client, ClientError};
use crate::daemon_ctl;

const OP_TIMEOUT: Duration = Duration::from_secs(30);

type CmdResult = Result<i32, String>;
type Library = (Vec<LibraryModel>, Vec<SlotAssignment>, Vec<ProviderStatus>);

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

// ---------------------------------------------------------------- ask

pub struct AskArgs {
    pub text: String,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub yes: bool,
}

pub fn ask(home: &Path, a: AskArgs) -> CmdResult {
    let mut c = connect(home, "marlowe-ask")?;
    if let Some(m) = &a.model {
        // The wire has no per-turn model; --model sets the Conductor slot, keeping its effort.
        let model: ModelRef = m.parse().map_err(|e| format!("--model: {e}"))?;
        let effort = current_effort(&mut c, &Slot::Conductor)?;
        send(&mut c, &Request::SetSlot { slot: Slot::Conductor, model: Some(model), effort })?;
        // Library ops run on daemon worker threads: wait until it is applied before asking.
        changed(&mut c)?;
    }
    send(
        &mut c,
        &Request::Ask {
            session: a.session.as_deref().map(SessionId::from),
            text: a.text,
            workspace: a.workspace,
            images: Vec::new(),
            options: None,
            client_ms: None,
        },
    )?;

    let mut mine: Option<SessionId> = a.session.as_deref().map(SessionId::from);
    let mut stdout = std::io::stdout();
    // Live per-token progress only on a terminal; piped output gets one line when thinking ends.
    let mut thinking = Thinking::new(stdout.is_terminal() && std::io::stderr().is_terminal());
    loop {
        let env = match c.next_event(Duration::from_secs(5)) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => return Err(e.to_string()),
        };
        let ev = env.event;
        // Only a seq-0 SessionOpened is the session our Ask created; seq > 0 is another client's.
        if mine.is_none() && env.seq == 0 {
            if let Event::SessionOpened { session, .. } = &ev {
                mine = Some(session.clone());
            }
        }
        match &ev {
            Event::Error { session, message } if session.is_none() || session.as_ref() == mine.as_ref() => {
                return Err(message.clone());
            }
            // Only this turn's approvals: a crew run's (`run` set) is answered in the TUI or with
            // `marlowe crew`, never by `--yes` or a closed stdin here.
            Event::ApprovalRequested { approval, session, tool, summary, detail, run: None, .. }
                if Some(session) == mine.as_ref() =>
            {
                let decision = if a.yes { ApprovalDecision::Allow } else { prompt(tool, summary, detail) };
                send(&mut c, &Request::Approve { approval: *approval, decision, reason: None })?;
            }
            _ => {}
        }
        if ev.session() != mine.as_ref() {
            continue;
        }
        match ev {
            Event::TextDelta { text, .. } => {
                if let Some(line) = thinking.end() {
                    eprintln!("{line}");
                }
                print!("{text}");
                let _ = stdout.flush();
            }
            // `tokens` counts this delta; an empty delta after the answer only settles the count.
            Event::ReasoningDelta { tokens, text, .. } => {
                if let Some(progress) = thinking.delta(tokens, !text.is_empty()) {
                    eprint!("{progress}");
                }
            }
            Event::Recalled { count, summary, .. } => {
                let n = format!("{count} memor{}", if count == 1 { "y" } else { "ies" });
                // Memory text: no control characters (escape sequences) reach the terminal.
                let first: String = summary.lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).collect();
                eprintln!("\x1b[2m⋯ recall · {n} · {first}\x1b[0m")
            }
            // The summary is `ToolHost::describe`, which already names the tool.
            Event::ToolStarted { tool, summary, .. } => {
                eprintln!("\x1b[2m⋯ {}\x1b[0m", if summary.is_empty() { tool } else { summary })
            }
            Event::ToolFinished { ok, summary, .. } => {
                eprintln!("\x1b[2m  {} {summary}\x1b[0m", if ok { "ok" } else { "failed" })
            }
            Event::TurnFinished { stop, usage, .. } => {
                if let Some(line) = thinking.end() {
                    eprintln!("{line}");
                }
                println!();
                eprintln!("\x1b[2m[{} in, {} out]\x1b[0m", usage.input, usage.output);
                return Ok(if stop == TurnStop::Done { 0 } else { 1 });
            }
            _ => {}
        }
    }
}

/// The `▸ thinking… N tokens` line. Live, it redraws per delta with `\r`; not live (stdout or
/// stderr is not a terminal), it prints nothing until thinking ends and then one plain line.
struct Thinking {
    live: bool,
    open: bool,
    tokens: u64,
}

impl Thinking {
    fn new(live: bool) -> Self {
        Self { live, open: false, tokens: 0 }
    }

    /// Counts a reasoning delta and returns the redraw to print, if any. `visible` is false for
    /// an empty delta that only settles the count.
    fn delta(&mut self, tokens: u64, visible: bool) -> Option<String> {
        self.tokens += tokens;
        if !visible {
            return None;
        }
        self.open = true;
        self.live.then(|| format!("\r\x1b[2m▸ thinking… {} tokens\x1b[0m", self.tokens))
    }

    /// Closes an open thinking line: a bare newline when live, else the one summary line.
    fn end(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.open) {
            return None;
        }
        Some(if self.live { String::new() } else { format!("▸ thought for {} tokens", self.tokens) })
    }
}

fn prompt(tool: &str, summary: &str, detail: &str) -> ApprovalDecision {
    eprintln!("\napproval: {tool}: {summary}");
    if !detail.is_empty() {
        eprintln!("{detail}");
    }
    eprint!("[y] allow  [s] allow for session  [N] deny: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        "s" => ApprovalDecision::AllowForSession,
        _ => ApprovalDecision::Deny,
    }
}

// ---------------------------------------------------------------- models

const MODELS_USAGE: &str = "usage: marlowe models [path | install <dir> | list | detect | catalog <provider> | \
add <provider> <model-id> | remove <ref> | slot <slot> <ref|default> [effort]]";

/// Where the memory models are looked for, and whether they are there.
fn models_path(home: &Path) -> CmdResult {
    let dir = marlowe_mind::resolve_models_dir(home);
    let found = marlowe_mind::has_models(&dir);
    println!("{}  ({})", dir.display(), if found { "memory models found" } else { "memory models MISSING: recall is keyword only" });
    if !found {
        println!(
            "fix: marlowe models install <dir holding {} and {}>",
            marlowe_mind::EMBEDDER_DIR,
            marlowe_mind::RERANKER_DIR
        );
    }
    Ok(0)
}

/// Copy the memory models into the fixed install location so every launch finds them.
fn models_install(src: Option<&String>) -> CmdResult {
    let src = src.ok_or("usage: marlowe models install <dir holding the memory models>")?;
    let fixed = marlowe_mind::install_dir().ok_or("no %LOCALAPPDATA% to install into")?;
    let dest = marlowe_mind::install_models(Path::new(src), &fixed).map_err(|e| e.to_string())?;
    println!("installed the memory models to {}", dest.display());
    Ok(0)
}

pub fn models(home: &Path, args: &[String]) -> CmdResult {
    match args.first().map(String::as_str) {
        Some("path") => return models_path(home),
        Some("install") => return models_install(args.get(1)),
        _ => {}
    }
    let mut c = connect(home, "marlowe-models")?;
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            print_library(&library(&mut c)?);
        }
        "detect" => {
            send(&mut c, &Request::DetectProviders)?;
            let (_, _, providers) = flatten(c.wait_for(OP_TIMEOUT, library_of))?;
            for p in providers {
                println!(
                    "{:<12} available={} configured={} detected={}{}",
                    p.kind.display_name(),
                    p.available_now,
                    p.configured,
                    p.detected,
                    p.note.map(|n| format!("  ({n})")).unwrap_or_default()
                );
            }
        }
        "catalog" => {
            let kind = provider_arg(args.get(1))?;
            send(&mut c, &Request::ListCatalog { provider: kind })?;
            let models = flatten(c.wait_for(OP_TIMEOUT, |e| match e {
                Event::Catalog { models, .. } => Some(models.clone()),
                _ => None,
            }))?;
            for m in models {
                println!(
                    "{}\tctx={}\ttools={}\treasoning={}\tprice={}",
                    m.id,
                    m.context_window.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                    m.supports_tools,
                    m.supports_reasoning,
                    m.pricing
                        .map(|p| format!("{}/{} per Mtok", p.input_per_mtok, p.output_per_mtok))
                        .unwrap_or_else(|| "-".into())
                );
            }
        }
        "add" => {
            let kind = provider_arg(args.get(1))?;
            let id = args.get(2).ok_or(MODELS_USAGE)?;
            send(&mut c, &Request::AddModel { provider: kind, model_id: id.clone() })?;
            print_library(&changed(&mut c)?);
        }
        "remove" => {
            let model: ModelRef = args.get(1).ok_or(MODELS_USAGE)?.parse().map_err(|e| format!("{e}"))?;
            send(&mut c, &Request::RemoveModel { model })?;
            print_library(&changed(&mut c)?);
        }
        "slot" => {
            let slot: Slot = args.get(1).ok_or(MODELS_USAGE)?.parse().map_err(|e| format!("{e}"))?;
            let model = match args.get(2).map(String::as_str) {
                None => return Err(MODELS_USAGE.into()),
                Some("default") => None,
                Some(r) => Some(r.parse::<ModelRef>().map_err(|e| format!("{e}"))?),
            };
            let effort = match args.get(3) {
                Some(e) => e.parse::<Effort>()?,
                None => current_effort(&mut c, &slot)?,
            };
            send(&mut c, &Request::SetSlot { slot, model, effort })?;
            print_library(&changed(&mut c)?);
        }
        other => return Err(format!("unknown models command {other:?}\n{MODELS_USAGE}")),
    }
    Ok(0)
}

fn library_of(e: &Event) -> Option<Library> {
    match e {
        Event::LibraryState { models, slots, providers } => {
            Some((models.clone(), slots.clone(), providers.clone()))
        }
        _ => None,
    }
}

/// Requests the library state; an `Error` from an earlier request surfaces here.
fn library(c: &mut Client) -> Result<Library, String> {
    send(c, &Request::LibraryState)?;
    flatten(c.wait_for(OP_TIMEOUT, library_of))
}

/// After a mutation: the daemon broadcasts a fresh `LibraryState` on success, or replies `Error`.
fn changed(c: &mut Client) -> Result<Library, String> {
    flatten(c.wait_for(OP_TIMEOUT, library_of))
}

fn current_effort(c: &mut Client, slot: &Slot) -> Result<Effort, String> {
    let (_, slots, _) = library(c)?;
    Ok(slots.iter().find(|s| &s.slot == slot).map(|s| s.effort).unwrap_or_default())
}

fn provider_arg(a: Option<&String>) -> Result<ProviderKind, String> {
    a.ok_or("a provider is required (ollama, openrouter)")?.parse::<ProviderKind>()
}

fn print_library((models, slots, _): &Library) {
    if models.is_empty() {
        println!("no models");
    }
    for m in models {
        println!("{}\t{}", m.model, m.info.display);
    }
    for s in slots {
        println!(
            "slot {:<18} {} · {}{}",
            s.slot.to_string(),
            s.model.as_ref().map(|m| m.to_string()).unwrap_or_else(|| "-".into()),
            s.effort,
            if s.inherited { " (default)" } else { "" }
        );
    }
}

// ---------------------------------------------------------------- sessions

pub fn sessions(home: &Path, args: &[String]) -> CmdResult {
    let limit: u32 = match args.first() {
        Some(n) => n.parse().map_err(|_| format!("usage: marlowe sessions [limit]; got {n:?}"))?,
        None => 20,
    };
    let mut c = connect(home, "marlowe-sessions")?;
    send(&mut c, &Request::ListSessions { limit })?;
    let list = flatten(c.wait_for(OP_TIMEOUT, |e| match e {
        Event::SessionList { sessions } => Some(sessions.clone()),
        _ => None,
    }))?;
    if list.is_empty() {
        println!("no sessions");
    }
    for s in list {
        println!(
            "{}\t{} turns\t{}\t{}",
            s.id,
            s.turns,
            s.workspace.as_deref().unwrap_or("-"),
            s.title
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------- status / shutdown

pub fn status(home: &Path) -> CmdResult {
    let Some(info) = daemon_ctl::live(home) else {
        println!("daemon: not running");
        return Ok(1);
    };
    let mut c = Client::connect(&info, "marlowe-status").map_err(|e| e.to_string())?;
    send(&mut c, &Request::Status)?;
    let r = flatten(c.wait_for(OP_TIMEOUT, |e| match e {
        Event::Status(r) => Some(r.clone()),
        _ => None,
    }))?;
    println!("daemon {} pid {} port {}", r.daemon_version, r.pid, info.port);
    println!("uptime {}s · {} clients · seq {}", r.uptime_ms / 1000, r.clients, r.seq);
    println!("active sessions: {}", r.active_sessions.len());
    println!("home {}", r.profile_home);
    Ok(0)
}

pub fn shutdown(home: &Path) -> CmdResult {
    let Some(info) = daemon_ctl::live(home) else {
        println!("daemon: not running");
        return Ok(0);
    };
    let mut c = Client::connect(&info, "marlowe-shutdown").map_err(|e| e.to_string())?;
    send(&mut c, &Request::Shutdown)?;
    // ShuttingDown, or the socket closing, both mean it heard us.
    match c.wait_for(Duration::from_secs(10), |e| matches!(e, Event::ShuttingDown).then_some(())) {
        Ok(_) | Err(ClientError::Closed) => {}
        Err(e) => return Err(e.to_string()),
    }
    println!("daemon: shutting down");
    Ok(0)
}

#[cfg(test)]
mod thinking_tests {
    use super::Thinking;

    #[test]
    fn piped_output_gets_one_thinking_line_not_per_token_redraws() {
        let mut t = Thinking::new(false);
        for _ in 0..50 {
            assert_eq!(t.delta(3, true), None);
        }
        assert_eq!(t.delta(2, false), None);
        assert_eq!(t.end().as_deref(), Some("▸ thought for 152 tokens"));
        assert_eq!(t.end(), None, "printed once");
    }

    #[test]
    fn a_terminal_gets_live_redraws_and_a_closing_newline() {
        let mut t = Thinking::new(true);
        let p = t.delta(4, true).unwrap();
        assert!(p.starts_with('\r') && p.contains("4 tokens"), "{p:?}");
        assert_eq!(t.end().as_deref(), Some(""));
        assert_eq!(t.end(), None);
        let mut quiet = Thinking::new(true);
        assert_eq!(quiet.delta(5, false), None);
        assert_eq!(quiet.end(), None, "no visible thinking, nothing to close");
    }
}
