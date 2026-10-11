//! Pull requests for crew work (track J): `Request::OpenPr`, answered with `PrOpened`.
//!
//! Push and PR creation never happen from a crew shell. The user asks the daemon: it takes the
//! run's branch (`crew/<id>` in the original checkout, or, once the run was merged and its branch
//! removed, the merged commit), asks **once** with the remote, branch, base, title and body, and
//! only then runs the `gh` tool's `pr_create` (which pushes without force and opens the PR). The
//! tool is the same one the conductor uses; the daemon reaches it through the tool factory, so it
//! holds no git or `gh` code of its own. Runs on its own thread, so the client's reader never
//! blocks on the approval.

use std::path::PathBuf;
use std::sync::Arc;

use marlowe_agent::{ToolCallReq, ToolCtx, ToolHost};
use marlowe_crew::{CrewRunInfo, CrewRunStatus, RunId};
use marlowe_model::CancelToken;
use marlowe_proto::{ApprovalDecision, Event, Request, SandboxKind, SessionId};
use serde_json::{json, Value};

use crate::daemon::Core;
use crate::HostRequest;

const NO_CREW: &str = "the crew is not running";
/// Runs searched when a client names a run by an id prefix.
const PREFIX_SCAN: u32 = 1_000;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let Request::OpenPr { run, title, body } = req else {
        crate::client::not_yet(core, client, &req, "J");
        return;
    };
    let c = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-open-pr".into()).spawn(move || {
        let reply = match open(&c, &run, title, body) {
            Ok((run, url)) => Event::PrOpened { run, url },
            Err(message) => Event::Error { session: None, message },
        };
        c.bus.reply(client, reply);
    });
    if let Err(e) = spawned {
        core.bus.reply(client, Event::Error { session: None, message: format!("could not start the pull request: {e}") });
    }
}

fn open(core: &Arc<Core>, run: &RunId, title: Option<String>, body: Option<String>) -> Result<(RunId, String), String> {
    let link = core.crew.get().ok_or(NO_CREW)?;
    let sup = &link.supervisor;
    let id = resolve(sup, run)?;
    let info = sup.get(&id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no run {}", id.short()))?;
    let origin = worktree_origin(&info)?;
    let host = (core.tools)(&HostRequest { workspace: Some(origin), model: None });
    let session = info.session.clone().unwrap_or_else(|| crate::crew::orphan_session(&id));
    let ask = |summary: String, detail: String| crate::crew::ask(core, &id, info.session.as_ref(), "gh pr_create".into(), summary, detail);
    let url = open_pr(host.as_ref(), &session, &info, title, body, &ask)?;
    Ok((id, url))
}

/// A full run id from the id itself or a unique prefix (8 characters is what clients show).
fn resolve(sup: &marlowe_crew::Supervisor, run: &RunId) -> Result<RunId, String> {
    let want = run.as_str().trim();
    if want.is_empty() {
        return Err("which run?".into());
    }
    if let Ok(Some(info)) = sup.get(&RunId(want.to_string())) {
        return Ok(info.id);
    }
    let hits: Vec<RunId> =
        sup.list(PREFIX_SCAN).map_err(|e| e.to_string())?.into_iter().map(|r| r.id).filter(|id| id.as_str().starts_with(want)).collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap_or_else(|| run.clone())),
        0 => Err(format!("no run {want}")),
        n => Err(format!("{n} runs start with {want}; give more of the id")),
    }
}

/// The original checkout a code run's worktree was cut from.
fn worktree_origin(info: &CrewRunInfo) -> Result<PathBuf, String> {
    if matches!(info.status, CrewRunStatus::Queued | CrewRunStatus::Running) {
        return Err(format!("run {} is {}; pause or cancel it first", info.id.short(), info.status.as_str()));
    }
    let sandbox = info.sandbox.as_ref().filter(|s| s.kind == SandboxKind::Worktree && s.branch.is_some());
    let Some(origin) = sandbox.and_then(|s| s.origin.as_deref()) else {
        return Err(format!("run {} has no git worktree, so there is no branch to propose", info.id.short()));
    };
    Ok(PathBuf::from(origin))
}

/// The commit a merged run ended at, from its note (`merged: fast-forward to abc1234`).
fn merged_commit(note: Option<&str>) -> Option<String> {
    let note = note.filter(|n| n.starts_with("merged"))?;
    note.split(|c: char| !c.is_ascii_hexdigit()).filter(|w| (7..=40).contains(&w.len())).last().map(String::from)
}

/// One run's PR, given the host that can run the `gh` tool and a way to ask the user. Asks exactly
/// once, with the plan; nothing is pushed or posted unless the answer is yes.
pub(crate) fn open_pr(
    host: &dyn ToolHost,
    session: &SessionId,
    info: &CrewRunInfo,
    title: Option<String>,
    body: Option<String>,
    ask: &dyn Fn(String, String) -> ApprovalDecision,
) -> Result<String, String> {
    let origin = worktree_origin(info)?;
    let branch = info.sandbox.as_ref().and_then(|s| s.branch.clone()).unwrap_or_default();
    let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| info.title.clone());
    let body = body.filter(|b| !b.trim().is_empty()).or_else(|| {
        info.result
            .as_ref()
            .map(|r| r.summary.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| format!("{s}\n\nOpened from crew run {}.", info.id.short()))
    });
    let mut args = json!({
        "op": "pr_create",
        "title": title,
        "head": branch,
        "cwd": origin.to_string_lossy(),
    });
    if let Some(b) = &body {
        args["body"] = json!(b);
    }
    if let Some(sha) = merged_commit(info.note.as_deref()) {
        // The merge removed the branch; propose the commit it ended at.
        args["head_sha"] = json!(sha);
    }
    let ctx = ToolCtx::new(session.clone(), 0, Some(origin), CancelToken::new(), false);
    let call = |args: &Value| host.call(&ToolCallReq { id: "open-pr".into(), name: "gh".into(), args: args.clone() }, &ctx);

    let mut plan_args = args.clone();
    plan_args["dry_run"] = json!(true);
    let plan = call(&plan_args);
    if plan.is_error {
        return Err(plan.content);
    }
    let field =
        |name: &str| plan.content.lines().find_map(|l| l.trim_start().strip_prefix(name)).map(|v| v.trim().to_string()).unwrap_or_default();
    let summary = format!(
        "open a pull request for run {}: push {} to {} and propose it into {}",
        info.id.short(),
        field("branch:").split(" @").next().unwrap_or(&branch),
        field("remote:"),
        field("into:").split(" (").next().unwrap_or("the base branch"),
    );
    if ask(summary, plan.content.clone()) == ApprovalDecision::Deny {
        return Err(format!("opening the pull request for run {} was not approved; nothing was pushed", info.id.short()));
    }
    let done = call(&args);
    if done.is_error {
        return Err(done.content);
    }
    done.content
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | ',')))
        .filter(|w| w.starts_with("http") && w.contains("/pull/"))
        .last()
        .map(String::from)
        .ok_or_else(|| format!("the pull request was opened but gh gave no URL:\n{}", done.content))
}

#[cfg(test)]
mod tests;
