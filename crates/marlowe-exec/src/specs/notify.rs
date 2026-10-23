//! The notify group (track N): `notify`, `send_file`, `notices`.
//!
//! `notify` reaches the user outside the conversation (a toast, a spoken line outside quiet hours,
//! a notice); `send_file` hands them a file (a card in the terminal with open and reveal);
//! `notices` is the agent's inbox of what the daemon noticed on its own (the Sentinel, CI, jobs,
//! monitors, schedules, crew runs, workflows, messages). The effects are the daemon's
//! (`marlowe_agent::Background`); these tools check and shape the arguments.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::background::{parse_urgency, NoticeQuery};
use marlowe_agent::{CallCtx, Effects, ToolGroup, ToolOutcome};
use serde_json::Value;

use super::{describe_by, ToolDef};
use crate::{first_line, int_arg, paths, str_arg, BuiltinTools};

/// Whether the group is live on a host: the daemon's conductor sessions configure it.
#[derive(Debug, Default)]
pub struct NotifyGroup {
    pub enabled: bool,
    /// Where crew runs keep what they made (under the profile home): `send_file` takes files from
    /// there as well as from the workspace, so a finished team task's report can be sent to the
    /// user from its worktree or output folder without a merge.
    pub run_roots: Vec<std::path::PathBuf>,
}

fn live(t: &BuiltinTools) -> bool {
    t.state::<NotifyGroup>().enabled
}

const NO_DAEMON: &str = "this needs the daemon, which this session does not have";

/// The largest file `send_file` copies.
pub const MAX_SEND_BYTES: u64 = 100 << 20;

fn notify_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("notify")
}

fn notify_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() else { return ToolOutcome::error(NO_DAEMON) };
    let title = str_arg(args, "title").map(str::trim).unwrap_or("");
    if title.is_empty() {
        return ToolOutcome::error("`title` is required");
    }
    let body = str_arg(args, "body").map(str::trim).unwrap_or("");
    let Some(urgency) = parse_urgency(str_arg(args, "urgency").unwrap_or("")) else {
        return ToolOutcome::error("`urgency` is low, normal or urgent");
    };
    match bg.0.notify(&cx.session, title, body, urgency) {
        Ok(done) => ToolOutcome::ok(done, "notified"),
        Err(e) => ToolOutcome::error(e),
    }
}

fn send_file_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("send_file")
}

fn send_file_call(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() else { return ToolOutcome::error(NO_DAEMON) };
    let Some(raw) = str_arg(args, "path").map(str::trim).filter(|p| !p.is_empty()) else {
        return ToolOutcome::error("`path` is required");
    };
    let root = tools.shell_root(cx);
    let abs = paths::resolve(root.as_deref(), raw);
    let in_a_run = tools.state::<NotifyGroup>().run_roots.iter().any(|r| paths::inside(r, &abs));
    if !in_a_run && !tools.trusted_read(&abs.to_string_lossy()) && tools.shell_dir.as_deref().is_none_or(|d| !paths::inside(d, &abs)) {
        return ToolOutcome::error(format!(
            "{} is outside the workspace and the sandbox; send_file only sends files from inside them or from a crew run's folders",
            abs.display()
        ));
    }
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) => return ToolOutcome::error(format!("cannot read {}: {e}", abs.display())),
    };
    if !meta.is_file() {
        return ToolOutcome::error(format!("{} is not a file", abs.display()));
    }
    if meta.len() > MAX_SEND_BYTES {
        return ToolOutcome::error(format!("{} is {} MB; send_file takes at most 100 MB", abs.display(), meta.len() >> 20));
    }
    let note = str_arg(args, "note").map(str::trim).unwrap_or("");
    match bg.0.send_file(&cx.session, &abs, note) {
        Ok(f) => ToolOutcome::ok(
            format!("Sent {} ({} bytes) to the user. The copy is {}; they see a card with open and reveal.", f.name, f.bytes, f.path),
            format!("sent {}", f.name),
        ),
        Err(e) => ToolOutcome::error(e),
    }
}

fn notices_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("notices")
}

fn notices_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() else { return ToolOutcome::error(NO_DAEMON) };
    let since_ms = match args.get("since") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(ms) => Some(ms),
            Err(_) => match marlowe_agent::cron::parse_local_time(s) {
                Some(ms) => Some(ms),
                None => return ToolOutcome::error(format!("`since` must be a local time like 2026-10-12T09:00 or Unix ms, not `{s}`")),
            },
        },
        Some(_) => return ToolOutcome::error("`since` must be a local time or Unix ms"),
    };
    let query = NoticeQuery {
        unread: args.get("unread").and_then(Value::as_bool).unwrap_or(false),
        since_ms,
        source: str_arg(args, "source").map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()),
        limit: int_arg(args, "limit").filter(|l| *l > 0).map(|l| (l as usize).min(100)).unwrap_or(20),
    };
    match bg.0.notices(&cx.session, &query) {
        Err(e) => ToolOutcome::error(e),
        Ok(list) if list.is_empty() => ToolOutcome::ok(if query.unread { "No unread notices." } else { "No notices." }, "none"),
        Ok(list) => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
            let lines: Vec<String> = list
                .iter()
                .map(|n| {
                    let age = super::tasks::fmt_elapsed(now - n.at_ms);
                    let detail = if n.detail.trim().is_empty() { String::new() } else { format!(" — {}", first_line(&n.detail, 160)) };
                    let head = format!("[{:?}] {:?}/{}", n.severity, n.source, n.kind).to_lowercase();
                    format!("{head} {}{detail} ({age} ago)", first_line(&n.title, 100))
                })
                .collect();
            ToolOutcome::ok(lines.join("\n"), format!("{} notices", list.len()))
        }
    }
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(notify_spec(), ToolGroup::Notify, Effects::NONE, |_, a, cx| notify_call(a, cx))
            .describe(|c| describe_by(c, "title"))
            .enabled(live),
        ToolDef::new(send_file_spec(), ToolGroup::Notify, Effects::READ, |t, a, cx| send_file_call(t, a, cx))
            .path_args(&["path"])
            .describe(|c| describe_by(c, "path"))
            .enabled(live),
        ToolDef::new(notices_spec(), ToolGroup::Notify, Effects::READ, |_, a, cx| notices_call(a, cx)).enabled(live),
    ]
}

/// Loop middleware for this group: none.
pub fn middleware(tools: &BuiltinTools) -> Vec<Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// `persona/blocks/tasks.md` describes both groups and is appended with the tasks group.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Turns the group on for a host the binary builds (the conductor's).
pub fn configure(tools: BuiltinTools, setup: &super::HostSetup<'_>) -> BuiltinTools {
    let run_roots = ["output", "scratch", "worktrees", "workflows"].iter().map(|d| setup.home.join(d)).collect();
    tools.with_state(NotifyGroup { enabled: true, run_roots })
}

/// Every conductor session, with or without a workspace (like the tasks group): a result the user
/// asked to be sent must be sendable from a conversation that has no workspace. A host that is not
/// the conductor's never configures the group, so its tools stay off ([`live`]).
pub fn detect(_root: Option<&Path>) -> bool {
    true
}

#[cfg(test)]
mod tests;
