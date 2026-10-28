//! `marlowe crew`: list, inspect, dispatch, steer and settle crew runs through the daemon.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use marlowe_proto::{
    ApprovalDecision, CrewRunInfo, CrewRunStatus, CrewStep, Event, Playbook, Request, RoleInfo, RunId, SessionId,
};

use crate::client::Client;
use crate::daemon_ctl;

const OP_TIMEOUT: Duration = Duration::from_secs(30);
/// A merge or discard waits on the user's approval (the daemon denies after 10 minutes).
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub const CREW_USAGE: &str = "\
usage: marlowe crew [list [N] | show <run> | roles
                    | dispatch <role> <goal> [--playbook build] [--workspace DIR] [--session ID]
                    | steer <run> <text> | cancel <run> | pause <run> | resume <run>
                    | merge <run> [--yes] | discard <run> [--yes]]
<run> is the id or a unique prefix of it (the 8 characters `list` shows).";

type CmdResult = Result<i32, String>;

pub(crate) fn connect(home: &Path) -> Result<Client, String> {
    let info = daemon_ctl::ensure(home)?;
    Client::connect(&info, "marlowe-crew").map_err(|e| e.to_string())
}

pub(crate) fn send(c: &mut Client, req: &Request) -> Result<(), String> {
    c.send(req).map_err(|e| e.to_string())
}

/// The next reply to this client (`seq: 0`) that `pick` accepts; a `seq: 0` `Error` fails.
/// Broadcasts (other runs' updates, other clients' errors) are skipped.
pub(crate) fn reply<T>(c: &mut Client, timeout: Duration, mut pick: impl FnMut(&Event) -> Option<T>) -> Result<T, String> {
    let end = Instant::now() + timeout;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out waiting for the daemon".into());
        }
        let Some(env) = c.next_event(left.min(Duration::from_secs(5))).map_err(|e| e.to_string())? else { continue };
        if env.seq != 0 {
            continue;
        }
        if let Event::Error { message, .. } = &env.event {
            return Err(message.clone());
        }
        if let Some(v) = pick(&env.event) {
            return Ok(v);
        }
    }
}

pub(crate) fn run_updated(e: &Event) -> Option<CrewRunInfo> {
    match e {
        Event::RunUpdated(info) => Some(info.clone()),
        _ => None,
    }
}

fn run_arg(args: &[String], i: usize) -> Result<RunId, String> {
    args.get(i).map(|s| RunId(s.trim().to_string())).ok_or_else(|| CREW_USAGE.to_string())
}

pub fn run(home: &Path, args: &[String]) -> CmdResult {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = args.get(1..).unwrap_or_default();
    match sub {
        "list" => list(home, rest),
        "show" => show(home, &run_arg(args, 1)?),
        "roles" => roles(home),
        "dispatch" => dispatch(home, rest),
        "steer" => {
            let run = run_arg(args, 1)?;
            let text = rest.get(1..).unwrap_or_default().join(" ");
            if text.trim().is_empty() {
                return Err(CREW_USAGE.into());
            }
            control(home, Request::Steer { run, text }, "steered")
        }
        "cancel" => control(home, Request::CancelRun { run: run_arg(args, 1)? }, "cancelled"),
        "pause" => control(home, Request::PauseRun { run: run_arg(args, 1)? }, "pausing"),
        "resume" => control(home, Request::ResumeRun { run: run_arg(args, 1)? }, "resumed"),
        "merge" | "discard" => {
            let run = run_arg(args, 1)?;
            let yes = rest.iter().any(|a| a == "--yes");
            let req = if sub == "merge" { Request::MergeRun { run: run.clone() } } else { Request::DiscardRun { run: run.clone() } };
            settle(home, req, &run, yes)
        }
        "-h" | "--help" | "help" => {
            println!("{CREW_USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown crew command {other:?}\n{CREW_USAGE}")),
    }
}

fn list(home: &Path, rest: &[String]) -> CmdResult {
    let limit: u32 = match rest.first() {
        Some(n) => n.parse().map_err(|_| format!("usage: marlowe crew list [N]; got {n:?}"))?,
        None => 20,
    };
    let mut c = connect(home)?;
    send(&mut c, &Request::ListRuns { limit })?;
    let runs = reply(&mut c, OP_TIMEOUT, |e| match e {
        Event::Runs { list } => Some(list.clone()),
        _ => None,
    })?;
    if runs.is_empty() {
        println!("no crew runs");
    }
    for r in &runs {
        println!("{}", run_line(r));
    }
    Ok(0)
}

fn roles(home: &Path) -> CmdResult {
    let mut c = connect(home)?;
    send(&mut c, &Request::ListRoles)?;
    let list: Vec<RoleInfo> = reply(&mut c, OP_TIMEOUT, |e| match e {
        Event::Roles { list } => Some(list.clone()),
        _ => None,
    })?;
    for r in list {
        let from = r.user_file.as_deref().map(|f| format!("  ({f})")).unwrap_or_default();
        println!("{:<11} {} · {} · {:?} sandbox{from}", r.name, r.slot, r.effort, r.sandbox);
        println!("            {}", r.description);
        println!("            tools: {}", r.tools.join(", "));
    }
    Ok(0)
}

fn dispatch(home: &Path, rest: &[String]) -> CmdResult {
    let (mut playbook, mut workspace, mut session) = (None, None, None);
    let mut words = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or(format!("{flag} needs a value"));
        match a.as_str() {
            "--playbook" => {
                let p = value("--playbook")?.parse::<Playbook>().map_err(|e| e.to_string())?;
                if p == Playbook::DeepResearch {
                    return Err(marlowe_crew::CrewError::Retired(p).to_string());
                }
                playbook = Some(p);
            }
            "--workspace" => workspace = Some(value("--workspace")?),
            "--session" => session = Some(SessionId::from(value("--session")?.as_str())),
            _ => words.push(a.clone()),
        }
    }
    let Some((role, goal)) = words.split_first() else { return Err(CREW_USAGE.into()) };
    let goal = goal.join(" ");
    if goal.trim().is_empty() {
        return Err(CREW_USAGE.into());
    }
    // Default workspace: the current directory, unless that is too broad to be one; with a
    // session the daemon uses the session's.
    let workspace = workspace.or_else(|| {
        session
            .is_none()
            .then(|| std::env::current_dir().ok())
            .flatten()
            .filter(|d| !marlowe_exec::paths::is_broad_root(d))
            .map(|d| d.display().to_string())
    });
    let mut c = connect(home)?;
    send(&mut c, &Request::Dispatch { session, role: role.clone(), goal, playbook, workspace, budget: None })?;
    let info = reply(&mut c, OP_TIMEOUT, run_updated)?;
    println!("{}", run_line(&info));
    if let Some(ws) = &info.workspace {
        println!("  workspace {ws}");
    }
    println!("  follow with `marlowe crew show {}`", info.id.short());
    Ok(0)
}

fn control(home: &Path, req: Request, done: &str) -> CmdResult {
    let mut c = connect(home)?;
    send(&mut c, &req)?;
    let info = reply(&mut c, OP_TIMEOUT, run_updated)?;
    println!("{done}: {}", run_line(&info));
    Ok(0)
}

/// Merge or discard: the daemon asks for approval on the bus; this prompts for it (or allows
/// with `--yes`), then prints the outcome.
fn settle(home: &Path, req: Request, run: &RunId, yes: bool) -> CmdResult {
    let mut c = connect(home)?;
    send(&mut c, &req)?;
    let end = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out waiting for the daemon".into());
        }
        let Some(env) = c.next_event(left.min(Duration::from_secs(5))).map_err(|e| e.to_string())? else { continue };
        match env.event {
            Event::ApprovalRequested { approval, tool, summary, detail, run: Some(asking), .. }
                if asking.as_str().starts_with(run.as_str()) =>
            {
                let decision = if yes { ApprovalDecision::Allow } else { prompt(&tool, &summary, &detail) };
                send(&mut c, &Request::Approve { approval, decision, reason: None })?;
            }
            Event::Error { message, .. } if env.seq == 0 => return Err(message),
            Event::RunUpdated(info) if env.seq == 0 => {
                println!("{}", info.note.as_deref().unwrap_or("done"));
                println!("{}", run_line(&info));
                return Ok(0);
            }
            _ => {}
        }
    }
}

fn prompt(tool: &str, summary: &str, detail: &str) -> ApprovalDecision {
    eprintln!("\napproval: {tool}: {summary}");
    if !detail.is_empty() {
        eprintln!("{detail}");
    }
    eprint!("[y] allow  [N] deny: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    }
}

fn show(home: &Path, run: &RunId) -> CmdResult {
    let mut c = connect(home)?;
    send(&mut c, &Request::WatchRun { run: run.clone() })?;
    let (info, steps) = reply(&mut c, OP_TIMEOUT, |e| match e {
        Event::RunTranscript { run, steps } => Some((run.clone(), steps.clone())),
        _ => None,
    })?;
    print!("{}", transcript(&info, &steps));
    Ok(0)
}

/// `● running` and friends: the rail's glyphs.
fn glyph(s: CrewRunStatus) -> &'static str {
    match s {
        CrewRunStatus::Running => "●",
        CrewRunStatus::NeedsYou => "◐",
        CrewRunStatus::Done => "✓",
        CrewRunStatus::Failed => "✗",
        CrewRunStatus::Queued => "○",
        CrewRunStatus::Paused => "‖",
        CrewRunStatus::Cancelled => "–",
    }
}

/// One line per run: id, status, role · title, step · spend · current action.
pub(crate) fn run_line(r: &CrewRunInfo) -> String {
    let playbook = r.playbook.map(|p| format!(" [{}]", p.as_str())).unwrap_or_default();
    let mut line = format!(
        "{}  {} {:<9} {} · {}{playbook} · step {} · {} tok · ${:.2}",
        r.id.short(),
        glyph(r.status),
        r.status.as_str().replace('_', " "),
        r.role,
        r.title,
        r.step,
        r.spent.tokens,
        r.spent.usd
    );
    if let Some(a) = r.current_action.as_deref().filter(|a| !a.is_empty()) {
        line.push_str(&format!(" · {a}"));
    }
    line
}

/// The `show` page: header, steps, then the result.
fn transcript(r: &CrewRunInfo, steps: &[CrewStep]) -> String {
    let mut out = String::new();
    let mut put = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    put(run_line(r));
    put(format!("run {}", r.id));
    put(format!("goal: {}", r.goal));
    if let Some(ws) = &r.workspace {
        put(format!("workspace: {ws}"));
    }
    if let Some(sb) = &r.sandbox {
        let branch = sb.branch.as_deref().map(|b| format!(" on {b}")).unwrap_or_default();
        put(format!("sandbox: {}{branch}", sb.root));
    }
    if let Some(n) = &r.note {
        put(format!("note: {n}"));
    }
    for s in steps {
        put(String::new());
        put(format!("── step {} · {} · {}", s.step, s.role, s.action));
        if !s.text.trim().is_empty() {
            put(s.text.trim_end().to_string());
        }
    }
    if let Some(d) = &r.result {
        put(String::new());
        put(format!("── result · confidence {:?}", d.confidence).to_lowercase());
        put(d.summary.trim_end().to_string());
        if let Some(diff) = &d.diff {
            put(format!("diff {}: {} files, +{} −{}", diff.branch, diff.files.len(), diff.insertions, diff.deletions));
            for f in &diff.files {
                put(format!("  {f}"));
            }
        }
        for a in &d.artifacts {
            put(format!("artifact {a}"));
        }
        for c in &d.citations {
            put(format!("- {} — {}", c.title, c.url_or_path));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_proto::{CrewBudget, CrewSpent, Dispatch};

    fn info() -> CrewRunInfo {
        CrewRunInfo {
            id: RunId("abcdef0123456789abcdef0123456789".into()),
            session: None,
            role: "researcher".into(),
            title: "sparse attention survey".into(),
            goal: "Survey sparse attention since 2023".into(),
            playbook: Some(Playbook::Build),
            status: CrewRunStatus::Running,
            workspace: None,
            sandbox: None,
            budget: CrewBudget::default(),
            spent: CrewSpent { tokens: 1200, steps: 3, usd: 0.042 },
            step: 3,
            current_action: Some("scouting 4 sub-questions".into()),
            note: None,
            tainted: false,
            created_ms: 0,
            updated_ms: 0,
            result: None,
            workflow: None,
        }
    }

    #[test]
    fn a_run_line_has_id_status_role_title_step_spend_action() {
        assert_eq!(
            run_line(&info()),
            "abcdef01  ● running   researcher · sparse attention survey [build] · step 3 · 1200 tok · $0.04 · scouting 4 sub-questions"
        );
    }

    #[test]
    fn the_show_page_ends_with_the_result() {
        let mut r = info();
        r.status = CrewRunStatus::Done;
        r.result = Some(Dispatch { summary: "Three families dominate.".into(), ..Default::default() });
        let steps = vec![CrewStep { step: 1, role: "researcher".into(), action: "plan".into(), text: "1. a\n2. b".into(), at_ms: 0 }];
        let page = transcript(&r, &steps);
        assert!(page.contains("── step 1 · researcher · plan\n1. a\n2. b\n"), "{page}");
        assert!(page.trim_end().ends_with("Three families dominate."), "{page}");
    }
}
