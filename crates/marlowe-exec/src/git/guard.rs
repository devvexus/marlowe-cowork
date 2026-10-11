//! The git group's loop middleware.
//!
//! - **Hard denials in every mode.** Force pushes in any spelling, hook skips and remote deletes of
//!   protected branches are refused for `git`, `gh`, and for `bash` / `powershell` command lines
//!   ([`policy`]). This runs ahead of the approval gate and again right before the call, so no
//!   mode, rule, hook rewrite or "allow always" reaches them.
//! - **The approval shows what will happen.** For `gh pr_create` and `git push` the resolved
//!   remote, branch, base, title and body are written into the call's arguments before the gate
//!   renders them (`before_approval` may rewrite arguments; the gate judges what runs), together
//!   with `expect_sha`, so the branch cannot move between the yes and the push. An impossible PR
//!   (no commits, on the base branch) is answered at once, without asking.

use marlowe_agent::{CallCtx, CallFlow, ToolMiddleware, ToolOutcome};
use marlowe_model::ToolCall;
use serde_json::json;

use super::git_tool::{flag, register_ci, remote_for, Ctx};
use super::{pr, policy, Env};
use crate::str_arg;

pub(crate) struct GitGuard {
    env: Env,
}

impl GitGuard {
    pub fn new(env: Env) -> Self {
        Self { env }
    }

    fn refusal(call: &ToolCall) -> Option<String> {
        match call.name.as_str() {
            "git" | "gh" | "ci_status" => policy::deny_structured(if call.name == "git" { "git" } else { "gh" }, &call.args),
            "bash" | "powershell" => str_arg(&call.args, "command").and_then(policy::deny_command),
            _ => None,
        }
    }
}

fn denied(why: String) -> CallFlow {
    CallFlow::Done(ToolOutcome::error(format!("refused: {why}")))
}

impl ToolMiddleware for GitGuard {
    fn name(&self) -> &str {
        "git-guard"
    }

    fn before_approval(&self, call: &mut ToolCall, cx: &CallCtx) -> CallFlow {
        if let Some(why) = Self::refusal(call) {
            return denied(why);
        }
        let op = str_arg(&call.args, "op").unwrap_or("").to_string();
        let is_pr = call.name == "gh" && op == "pr_create" && !flag(&call.args, "dry_run");
        let is_push = call.name == "git" && op == "push";
        if !is_pr && !is_push {
            return CallFlow::Continue;
        }
        let dir = match self.env.dir(cx.workspace.as_deref(), str_arg(&call.args, "cwd")) {
            Ok(d) => d,
            Err(e) => return CallFlow::Done(ToolOutcome::error(e)),
        };
        let cancel = || cx.cancel.is_cancelled();
        let c = Ctx { env: &self.env, dir: &dir, cancel: &cancel };
        if is_pr {
            return match pr::plan(&c, &call.args) {
                Ok(plan) => {
                    call.args = plan.materialize(&call.args);
                    CallFlow::Continue
                }
                Err(e) => CallFlow::Done(ToolOutcome::error(e)),
            };
        }
        let Some(branch) = str_arg(&call.args, "branch").filter(|b| !b.is_empty()).map(String::from).or_else(|| c.branch()) else {
            return CallFlow::Continue;
        };
        let remote = remote_for(&c, &call.args, &branch);
        if let Some(o) = call.args.as_object_mut() {
            o.insert("branch".into(), json!(branch));
            o.insert("remote".into(), json!(remote));
        }
        CallFlow::Continue
    }

    /// A `git push` the model made through the shell is watched like one made through the tool.
    fn after_call(&self, call: &ToolCall, cx: &CallCtx, outcome: &mut ToolOutcome) {
        if outcome.is_error || !matches!(call.name.as_str(), "bash" | "powershell") {
            return;
        }
        if !str_arg(&call.args, "command").is_some_and(policy::pushes) {
            return;
        }
        let Ok(dir) = self.env.dir(cx.workspace.as_deref(), str_arg(&call.args, "cwd")) else { return };
        let cancel = || cx.cancel.is_cancelled();
        let c = Ctx { env: &self.env, dir: &dir, cancel: &cancel };
        if let Some(branch) = c.branch() {
            let remote = remote_for(&c, &call.args, &branch);
            register_ci(&c, &call.args, &branch, &remote);
        }
    }

    fn before_call(&self, call: &ToolCall, _cx: &CallCtx) -> CallFlow {
        // Whatever a later middleware rewrote the arguments into is judged again.
        match Self::refusal(call) {
            Some(why) => denied(why),
            None => CallFlow::Continue,
        }
    }
}
