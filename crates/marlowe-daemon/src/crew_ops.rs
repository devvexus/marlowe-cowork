//! Crew requests. Replies go to the asking client with `seq: 0`; the supervisor's own
//! `RunUpdated` / `RunStep` / `RunFinished` broadcasts reach everyone. Merge and discard raise an
//! approval first, on their own thread so the client's reader never blocks.

use std::sync::Arc;

use marlowe_crew::{CrewError, CrewRunInfo, CrewRunStatus, DispatchArgs, MergeOutcome, RunId, Supervisor};
use marlowe_proto::{ApprovalDecision, Event, Request, SessionId};

use crate::crew::CrewLink;
use crate::daemon::Core;

const NO_CREW: &str = "the crew is not running";
/// Runs searched when a client names a run by an id prefix.
const PREFIX_SCAN: u32 = 1_000;
const LIST_MAX: u32 = 500;

/// Handles a crew request from `client`.
pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    let fail = |message: String| reply(Event::Error { session: None, message });
    let Some(link) = core.crew.get() else {
        match req {
            Request::ListRoles => reply(Event::Roles { list: marlowe_crew::Roles::load(&core.home).infos() }),
            _ => fail(NO_CREW.into()),
        }
        return;
    };
    let sup = &link.supervisor;
    match req {
        Request::ListRoles => reply(Event::Roles { list: sup.reload_roles() }),
        Request::Dispatch { session, role, goal, playbook, workspace, budget } => {
            match start_run(core, sup, session, DispatchArgs { role, goal, playbook, workspace, budget, tainted: false }) {
                Ok(info) => reply(Event::RunUpdated(info)),
                Err(m) => fail(m),
            }
        }
        Request::ListRuns { limit } => match sup.list(limit.clamp(1, LIST_MAX)) {
            Ok(list) => reply(Event::Runs { list }),
            Err(e) => fail(e.to_string()),
        },
        Request::WatchRun { run } => match resolve(sup, &run).and_then(|id| sup.transcript(&id).map_err(|e| e.to_string())) {
            Ok((run, steps)) => reply(Event::RunTranscript { run, steps }),
            Err(m) => fail(m),
        },
        Request::Steer { run, text } => {
            if text.trim().is_empty() {
                return fail("nothing to steer with".into());
            }
            control(core, client, sup, &run, |id| sup.steer(id, text.trim()))
        }
        // Cancel first, so a run woken by the denial already sees its cancel and ends cancelled.
        Request::CancelRun { run } => control(core, client, sup, &run, |id| {
            let r = sup.cancel(id);
            link.deny_pending(core, id);
            r
        }),
        Request::PauseRun { run } => control(core, client, sup, &run, |id| sup.pause(id)),
        Request::ResumeRun { run } => control(core, client, sup, &run, |id| sup.resume(id)),
        Request::MergeRun { run } => spawn_settle(core, client, run, Settle::Merge),
        Request::DiscardRun { run } => spawn_settle(core, client, run, Settle::Discard),
        other => fail(format!("not a crew request: {other:?}")),
    }
}

/// Queues a run. The workspace is the argument, else the parent session's.
fn start_run(core: &Core, sup: &Supervisor, session: Option<SessionId>, mut args: DispatchArgs) -> Result<CrewRunInfo, String> {
    if args.role.trim().is_empty() || args.goal.trim().is_empty() {
        return Err("a dispatch needs a role and a goal".into());
    }
    if let Some(id) = &session {
        let summary = core.store.get(id)?.ok_or_else(|| format!("unknown session {id}"))?;
        if args.workspace.is_none() {
            args.workspace = summary.workspace;
        }
    }
    let receipt = sup.dispatch(session, args).map_err(|e| e.to_string())?;
    sup.get(&receipt.run_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("run {} vanished after dispatch", receipt.run_id.short()))
}

/// A full run id, from the id itself or a unique prefix of it (8 characters is what clients show).
fn resolve(sup: &Supervisor, run: &RunId) -> Result<RunId, String> {
    let want = run.as_str().trim();
    if want.is_empty() {
        return Err("which run?".into());
    }
    if let Ok(Some(info)) = sup.get(&RunId(want.to_string())) {
        return Ok(info.id);
    }
    let runs = sup.list(PREFIX_SCAN).map_err(|e| e.to_string())?;
    let hits: Vec<RunId> = runs.into_iter().map(|r| r.id).filter(|id| id.as_str().starts_with(want)).collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap_or_else(|| run.clone())),
        0 => Err(format!("no run {want}")),
        n => Err(format!("{n} runs start with {want}; give more of the id")),
    }
}

/// Steer, cancel, pause, resume: apply, then reply with the run as it now stands.
fn control(core: &Core, client: u64, sup: &Supervisor, run: &RunId, op: impl FnOnce(&RunId) -> Result<(), CrewError>) {
    let result = resolve(sup, run).and_then(|id| {
        op(&id).map_err(|e| e.to_string())?;
        sup.get(&id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no run {}", id.short()))
    });
    match result {
        Ok(info) => core.bus.reply(client, Event::RunUpdated(info)),
        Err(message) => core.bus.reply(client, Event::Error { session: None, message }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    Merge,
    Discard,
}

fn spawn_settle(core: &Arc<Core>, client: u64, run: RunId, what: Settle) {
    let c = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-crew-settle".into()).spawn(move || {
        let Some(link) = c.crew.get() else { return };
        let result = settle(&c, link, &run, what);
        match result {
            Ok(info) => c.bus.reply(client, Event::RunUpdated(info)),
            Err(message) => c.bus.reply(client, Event::Error { session: None, message }),
        }
    });
    if let Err(e) = spawned {
        core.bus.reply(client, Event::Error { session: None, message: format!("could not start the {what:?}: {e}") });
    }
}

/// Asks the user, then merges (or discards). Only the daemon merges, and never pushes.
fn settle(core: &Core, link: &CrewLink, run: &RunId, what: Settle) -> Result<CrewRunInfo, String> {
    let sup = &link.supervisor;
    let id = resolve(sup, run)?;
    let info = sup.get(&id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no run {}", id.short()))?;
    if matches!(info.status, CrewRunStatus::Queued | CrewRunStatus::Running) {
        return Err(format!("run {} is {}; pause or cancel it first", id.short(), info.status.as_str()));
    }
    if what == Settle::Merge && info.result.is_none() {
        return Err(format!("run {} has no result to merge", id.short()));
    }
    let (tool, summary) = settle_prompt(&info, what);
    let detail = settle_detail(&info);
    let decision = crate::crew::ask(core, &id, info.session.as_ref(), tool, summary, detail);
    if decision == ApprovalDecision::Deny {
        return Err(format!("{} of run {} was not approved", verb(what), id.short()));
    }
    let said = match what {
        Settle::Merge => outcome_line(&sup.merge(&id).map_err(|e| e.to_string())?),
        Settle::Discard => {
            sup.discard(&id).map_err(|e| e.to_string())?;
            "discarded".into()
        }
    };
    // The reply's note says what happened; the broadcast copy keeps the supervisor's own note.
    let mut info = sup.get(&id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no run {}", id.short()))?;
    info.note = Some(said);
    Ok(info)
}

fn verb(what: Settle) -> &'static str {
    match what {
        Settle::Merge => "the merge",
        Settle::Discard => "the discard",
    }
}

/// The approval's tool name and one-line summary.
fn settle_prompt(info: &CrewRunInfo, what: Settle) -> (String, String) {
    let short = info.id.short();
    match (what, info.result.as_ref().and_then(|d| d.diff.as_ref())) {
        (Settle::Merge, Some(diff)) => (
            "crew_merge".into(),
            format!(
                "merge {} into the checkout's current branch: {} files, +{} −{} (never pushed)",
                diff.branch,
                diff.files.len(),
                diff.insertions,
                diff.deletions
            ),
        ),
        (Settle::Merge, None) => ("crew_merge".into(), format!("accept run {short}'s report into the workspace's research folder")),
        (Settle::Discard, _) => {
            let what = match info.sandbox.as_ref().and_then(|s| s.branch.as_deref()) {
                Some(branch) => format!("its worktree and branch {branch}"),
                None => "its scratch directory".into(),
            };
            ("crew_discard".into(), format!("discard run {short}: delete {what} without merging"))
        }
    }
}

fn settle_detail(info: &CrewRunInfo) -> String {
    let mut lines = vec![format!("{} · {} · {}", info.role, info.title, info.status.as_str())];
    if let Some(s) = &info.sandbox {
        lines.push(format!("sandbox {}", s.root));
        if let Some(o) = &s.origin {
            lines.push(format!("origin {o}"));
        }
    }
    if let Some(diff) = info.result.as_ref().and_then(|d| d.diff.as_ref()) {
        lines.extend(diff.files.iter().map(|f| format!("  {f}")));
    }
    lines.join("\n")
}

fn outcome_line(o: &MergeOutcome) -> String {
    match o {
        MergeOutcome::FastForward { head } => format!("fast-forwarded to {head}"),
        MergeOutcome::MergeCommit { commit } => format!("merged as {commit}"),
        MergeOutcome::NothingToMerge => "nothing to merge".into(),
        MergeOutcome::ReportCopied { path } => format!("report copied to {}", path.display()),
    }
}
