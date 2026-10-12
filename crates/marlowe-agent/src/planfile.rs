//! The plan file (M13): `enter_plan` switches to plan mode, `exit_plan` writes
//! `<home>\plans\<session>-<slug>.md`, the user may edit it, and the approved text (as edited) is
//! pinned in the working set.
//!
//! Plan mode is read-only. When the model has a plan it calls `exit_plan{plan}`: the plan goes to
//! a file, the clients are told (`Event::PlanReady`: path and text) and the call waits for
//! `Request::AnswerPlan`. The modal can open the file in `$EDITOR`, so what the user approves may
//! differ from what the model wrote; the tool reads the file again after the answer, and that
//! text is what the model is told and what compaction keeps ([`PlanState`], rendered by
//! `todo::pinned_block`). Approval names the next mode (`accept_edits` or `default`); a rejection
//! keeps `plan` and returns the user's feedback.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_model::CancelToken;
use marlowe_proto::{PermissionMode, PlanFileDecision, SessionId};
use serde_json::Value;

use crate::approval::{ModeSink, Permissions};
use crate::todo::PlanState;
use crate::tools::{CallCtx, ToolOutcome};

/// Where plan files go (a service; the daemon passes `<home>\plans`).
#[derive(Debug, Clone)]
pub struct PlanDir(pub PathBuf);

/// Waits for the user's answer to a plan: the daemon's modal round trip.
pub trait PlanAsker: Send + Sync {
    /// `Err` when nobody answered (no client, timeout, interrupt).
    fn ask(&self, session: &SessionId, path: &Path, text: &str, cancel: &CancelToken) -> Result<PlanFileDecision, String>;
}

/// The plan approval service in `Services`.
#[derive(Clone)]
pub struct PlanReview(pub Arc<dyn PlanAsker>);

/// What `exit_plan` returns where nobody can approve (headless, crew).
pub const NO_ONE_TO_APPROVE: &str = "No one can approve a plan in this session, so plan mode stays on. Say what you would \
change as text in your reply, and stop; the user will review it.";

/// A file name stem from a plan's first line: `# Fix the login race` -> `fix-the-login-race`.
pub fn slug(plan: &str) -> String {
    let first = plan.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("plan");
    let mut out = String::new();
    for c in first.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "plan".into()
    } else {
        out
    }
}

/// `<dir>\<session>-<slug>.md` (the session id is made safe for a file name).
pub fn plan_path(dir: &Path, session: &SessionId, plan: &str) -> PathBuf {
    let safe: String = session.as_str().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    dir.join(format!("{safe}-{}.md", slug(plan)))
}

/// The mode an approval may move to: only `accept_edits` or `default` (a plan's approval never
/// widens the session to `bypass` or `dont_ask`).
pub fn next_mode(requested: PermissionMode) -> PermissionMode {
    match requested {
        PermissionMode::AcceptEdits => PermissionMode::AcceptEdits,
        _ => PermissionMode::Default,
    }
}

fn announce(cx: &CallCtx, mode: PermissionMode) {
    if let Some(sink) = cx.scope.services.get::<ModeSink>() {
        (sink.0)(&cx.session, mode);
    }
}

/// The `enter_plan` tool: the approval gate asked first (its consequence is Ask), so running it
/// means the user agreed to read-only planning.
pub fn enter_plan_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(perms) = cx.scope.services.get::<Permissions>() else {
        return ToolOutcome::error("plan mode is not available in this session");
    };
    if perms.mode() == PermissionMode::Plan {
        return ToolOutcome::ok("Already in plan mode. Explore, then call exit_plan with the plan.", "already planning");
    }
    perms.set_mode(PermissionMode::Plan);
    announce(cx, PermissionMode::Plan);
    let why = args.get("reason").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty());
    ToolOutcome::ok(
        format!(
            "Plan mode is on{}: nothing can be changed or run until the plan is approved. Read the code, work out the approach, \
             then call exit_plan with the steps and the files they change.",
            why.map(|w| format!(" ({w})")).unwrap_or_default()
        ),
        "plan mode on",
    )
}

/// The `exit_plan` tool.
pub fn exit_plan_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(plan) = args.get("plan").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()) else {
        return ToolOutcome::error("`plan` is required: the steps you will take and the files you will change");
    };
    let perms = cx.scope.services.get::<Permissions>();
    if perms.is_some_and(|p| p.mode() != PermissionMode::Plan) {
        return ToolOutcome::error("Not in plan mode, so there is no plan to approve. Carry on with the work, or call enter_plan first.");
    }
    let dir = cx.scope.services.get::<PlanDir>().map(|d| d.0.clone()).unwrap_or_else(|| std::env::temp_dir().join("marlowe-plans"));
    let path = plan_path(&dir, &cx.session, plan);
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, format!("{plan}\n"))) {
        return ToolOutcome::error(format!("could not write the plan file {}: {e}", path.display()));
    }
    let Some(review) = cx.scope.services.get::<PlanReview>() else {
        return ToolOutcome { summary: "no one to approve".into(), ..ToolOutcome::error(NO_ONE_TO_APPROVE) };
    };
    let decision = match review.0.ask(&cx.session, &path, plan, &cx.cancel) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::error(format!("The plan was not answered ({e}). {NO_ONE_TO_APPROVE}")),
    };
    match decision {
        PlanFileDecision::Reject { feedback } => {
            let note = feedback.as_deref().map(str::trim).filter(|f| !f.is_empty());
            ToolOutcome::ok(
                format!(
                    "The user did not approve the plan; plan mode stays on.{} Revise the plan and call exit_plan again, or ask \
                     the user what they want.",
                    note.map(|f| format!(" Their feedback: {f}")).unwrap_or_default()
                ),
                "plan rejected",
            )
        }
        PlanFileDecision::Approve { mode } => {
            let mode = next_mode(mode);
            // What the user approved is the file as it is now (they may have edited it).
            let approved = std::fs::read_to_string(&path).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| plan.to_string());
            let edited = approved != plan;
            cx.scope.with_ext(|e| {
                *e.get_or_default::<PlanState>() = PlanState { plan: Some(approved.clone()), approved: true };
            });
            if let Some(p) = perms {
                p.set_mode(mode);
                announce(cx, mode);
            }
            let mode_name = match mode {
                PermissionMode::AcceptEdits => "accept_edits",
                _ => "default",
            };
            let body = if edited {
                format!("The user approved the plan with their own edits; follow this version, not yours:\n\n{approved}\n\n")
            } else {
                "The user approved the plan. ".to_string()
            };
            ToolOutcome::ok(
                format!("{body}Plan mode is off (now {mode_name}). Carry out the plan and keep the todo list current."),
                if edited { "plan approved (edited)" } else { "plan approved" },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::tools::{SessionScope, ToolCtx};
    use crate::Services;

    /// An asker that edits the file as the user would, then answers.
    struct Scripted {
        edit: Option<&'static str>,
        decision: PlanFileDecision,
        seen: Mutex<Vec<(PathBuf, String)>>,
    }

    impl PlanAsker for Scripted {
        fn ask(&self, _s: &SessionId, path: &Path, text: &str, _c: &CancelToken) -> Result<PlanFileDecision, String> {
            self.seen.lock().unwrap().push((path.to_path_buf(), text.to_string()));
            if let Some(e) = self.edit {
                std::fs::write(path, e).unwrap();
            }
            Ok(self.decision.clone())
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("marlowe-planfile-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session_cx(home: &Path, asker: Option<Arc<Scripted>>, mode: PermissionMode) -> (CallCtx, Permissions, Arc<Mutex<Vec<PermissionMode>>>) {
        let perms = Permissions::with_parser(home, None, None, None);
        perms.set_mode(mode);
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        let mut services = Services::new()
            .with(perms.clone())
            .with(PlanDir(home.join("plans")))
            .with(ModeSink(Arc::new(move |_s: &SessionId, m| h.lock().unwrap().push(m))));
        if let Some(a) = asker {
            services = services.with(PlanReview(a));
        }
        let tool = ToolCtx::new(SessionId("s-1".into()), 0, None, CancelToken::new(), false);
        (CallCtx::new(tool, SessionScope { ext: Default::default(), services }), perms, heard)
    }

    fn scripted(edit: Option<&'static str>, decision: PlanFileDecision) -> Arc<Scripted> {
        Arc::new(Scripted { edit, decision, seen: Mutex::new(Vec::new()) })
    }

    #[test]
    fn parity_m13_the_users_edit_to_the_file_is_what_gets_pinned_and_the_chosen_mode_applies() {
        let home = tmp("edit");
        let asker = scripted(Some("# Fix the race\n1. add the lock\n2. skip the migration\n"), PlanFileDecision::Approve { mode: PermissionMode::AcceptEdits });
        let (cx, perms, heard) = session_cx(&home, Some(asker.clone()), PermissionMode::Plan);
        let out = exit_plan_call(&json!({"plan": "# Fix the race\n1. add the lock\n2. run the migration"}), &cx);
        assert!(!out.is_error, "{}", out.content);
        // The file was written under <home>\plans\<session>-<slug>.md before the user saw it.
        let seen = asker.seen.lock().unwrap();
        assert_eq!(seen[0].0, home.join("plans").join("s-1-fix-the-race.md"));
        assert!(seen[0].1.contains("run the migration"));
        // The pinned plan is the edited text, and the model is told so.
        let pinned = cx.scope.with_ext(|e| e.get::<PlanState>().cloned()).unwrap();
        assert!(pinned.approved && pinned.plan.as_deref().unwrap().contains("skip the migration"), "{pinned:?}");
        assert!(!pinned.plan.unwrap().contains("run the migration"));
        assert!(out.content.contains("own edits") && out.content.contains("skip the migration"), "{}", out.content);
        // The chosen mode is applied and announced.
        assert_eq!(perms.mode(), PermissionMode::AcceptEdits);
        assert_eq!(*heard.lock().unwrap(), [PermissionMode::AcceptEdits]);
    }

    #[test]
    fn parity_m13_an_approval_of_the_plan_as_written_goes_to_default_and_never_widens() {
        let home = tmp("plain");
        for (asked, expect) in [
            (PermissionMode::Default, PermissionMode::Default),
            (PermissionMode::Bypass, PermissionMode::Default),
            (PermissionMode::DontAsk, PermissionMode::Default),
            (PermissionMode::AcceptEdits, PermissionMode::AcceptEdits),
        ] {
            let (cx, perms, _) = session_cx(&home, Some(scripted(None, PlanFileDecision::Approve { mode: asked })), PermissionMode::Plan);
            let out = exit_plan_call(&json!({"plan": "1. do it"}), &cx);
            assert!(!out.is_error && out.content.contains("approved the plan"), "{}", out.content);
            assert_eq!(perms.mode(), expect, "asked {asked:?}");
        }
    }

    #[test]
    fn parity_m13_a_rejection_keeps_plan_mode_and_returns_the_feedback() {
        let home = tmp("reject");
        let (cx, perms, heard) = session_cx(
            &home,
            Some(scripted(None, PlanFileDecision::Reject { feedback: Some("split step 2".into()) })),
            PermissionMode::Plan,
        );
        let out = exit_plan_call(&json!({"plan": "1. one big step"}), &cx);
        assert!(out.content.contains("did not approve") && out.content.contains("split step 2"), "{}", out.content);
        assert_eq!(perms.mode(), PermissionMode::Plan);
        assert!(heard.lock().unwrap().is_empty(), "no mode change to announce");
        assert!(cx.scope.with_ext(|e| e.get::<PlanState>().is_none()), "nothing pinned");
    }

    #[test]
    fn exit_plan_outside_plan_mode_and_without_an_approver_do_nothing() {
        let home = tmp("guard");
        let (cx, perms, _) = session_cx(&home, Some(scripted(None, PlanFileDecision::Approve { mode: PermissionMode::Default })), PermissionMode::Default);
        let out = exit_plan_call(&json!({"plan": "x"}), &cx);
        assert!(out.is_error && out.content.contains("Not in plan mode"));
        assert_eq!(perms.mode(), PermissionMode::Default);
        let (cx, perms, _) = session_cx(&home, None, PermissionMode::Plan);
        let out = exit_plan_call(&json!({"plan": "x"}), &cx);
        assert!(out.is_error && out.content.contains("No one can approve"), "{}", out.content);
        assert_eq!(perms.mode(), PermissionMode::Plan);
        assert!(exit_plan_call(&json!({}), &cx).is_error);
    }

    #[test]
    fn enter_plan_switches_to_plan_and_remembers_where_it_came_from() {
        let home = tmp("enter");
        let (cx, perms, heard) = session_cx(&home, None, PermissionMode::AcceptEdits);
        let out = enter_plan_call(&json!({"reason": "a risky migration"}), &cx);
        assert!(!out.is_error && out.content.contains("a risky migration"), "{}", out.content);
        assert_eq!(perms.mode(), PermissionMode::Plan);
        assert_eq!(*heard.lock().unwrap(), [PermissionMode::Plan]);
        let again = enter_plan_call(&json!({}), &cx);
        assert!(again.content.contains("Already"), "{}", again.content);
        assert_eq!(heard.lock().unwrap().len(), 1);
    }

    #[test]
    fn slugs_are_short_and_safe() {
        assert_eq!(slug("# Fix the login race\nmore"), "fix-the-login-race");
        assert_eq!(slug("\n\n  ***  "), "plan");
        assert!(slug(&"word ".repeat(30)).len() <= 40);
        let p = plan_path(Path::new("C:/h/plans"), &SessionId("a/b:c".into()), "Do it");
        assert!(p.ends_with("a_b_c-do-it.md"), "{p:?}");
    }
}
