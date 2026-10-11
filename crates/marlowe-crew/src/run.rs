//! The durable run record and its per-step checkpoints.

use std::path::PathBuf;

use marlowe_model::{Message, Part, Role, ToolCall, Usage};
use marlowe_proto::{CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, CrewStep, Dispatch, Playbook, RunId, SessionId};

use serde::{Deserialize, Serialize};

use crate::role::RoleCard;
use crate::sandbox::Sandbox;

/// One crew run. The supervisor owns the live copy; `crew_runs` holds the durable one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    /// The conductor session that dispatched it.
    pub session: Option<SessionId>,
    pub role: String,
    pub title: String,
    pub goal: String,
    pub playbook: Option<Playbook>,
    pub status: CrewRunStatus,
    /// The original workspace (read-only to the crew).
    pub workspace: Option<PathBuf>,
    /// Created when the run first starts; `None` while queued.
    pub sandbox: Option<Sandbox>,
    /// The role's budget overlaid with the dispatch's; raised by an approved extension.
    pub budget: CrewBudget,
    pub spent: CrewSpent,
    /// Agent steps completed.
    pub step: u32,
    pub current_action: Option<String>,
    /// Why it needs the user, or why it failed.
    pub note: Option<String>,
    /// Untrusted content (web, paper, search results) has entered the run. Shell commands then
    /// ask, and the run's memories are written at untrusted trust.
    pub tainted: bool,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub result: Option<Dispatch>,
    /// The result (or failure) has not reached the parent session yet. Set when the run ends with
    /// a session to tell, cleared by [`Supervisor::mark_relayed`]; the daemon relays every run
    /// still due at boot.
    ///
    /// [`Supervisor::mark_relayed`]: crate::Supervisor::mark_relayed
    #[serde(default)]
    pub relay_due: bool,
}

impl Run {
    /// A queued run for `role`. `budget` overrides the role's budget field by field.
    pub fn new(
        session: Option<SessionId>,
        role: &RoleCard,
        goal: &str,
        playbook: Option<Playbook>,
        workspace: Option<PathBuf>,
        budget: CrewBudget,
    ) -> Run {
        let now = crate::now_ms();
        Run {
            id: RunId::new_random(),
            session,
            role: role.name.clone(),
            title: title_from_goal(goal),
            goal: goal.trim().to_string(),
            playbook,
            status: CrewRunStatus::Queued,
            workspace,
            sandbox: None,
            budget: role.budget.overlay(budget),
            spent: CrewSpent::default(),
            step: 0,
            current_action: None,
            note: None,
            tainted: false,
            created_ms: now,
            updated_ms: now,
            result: None,
            relay_due: false,
        }
    }

    /// The wire view.
    pub fn info(&self) -> CrewRunInfo {
        CrewRunInfo {
            id: self.id.clone(),
            session: self.session.clone(),
            role: self.role.clone(),
            title: self.title.clone(),
            goal: self.goal.clone(),
            playbook: self.playbook,
            status: self.status,
            workspace: self.workspace.as_ref().map(|p| p.display().to_string()),
            sandbox: self.sandbox.as_ref().map(Sandbox::info),
            budget: self.budget,
            spent: self.spent,
            step: self.step,
            current_action: self.current_action.clone(),
            note: self.note.clone(),
            tainted: self.tainted,
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
            result: self.result.clone(),
        }
    }
}

/// The goal's first line, cut at a word boundary to at most 60 chars.
pub fn title_from_goal(goal: &str) -> String {
    const MAX: usize = 60;
    let line = goal.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX).collect();
    let cut = match cut.rfind(' ') {
        Some(i) if i > MAX / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{}…", cut.trim_end_matches([',', ';', ':', '.', ' ']))
}

/// One `crew_run_steps` row: one agent step of one playbook step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    pub run: RunId,
    /// Agent step number within the run, from 1.
    pub step: u32,
    /// The playbook step it belongs to: `plan`, `scout:2`, `verify`, `synthesize`, `implement`,
    /// `test:3`, `review`, `fix`, or `main` for a role-prompt dispatch.
    pub key: String,
    pub role: String,
    /// One line: what the step did.
    pub action: String,
    /// Messages appended to the playbook step's conversation since the previous row.
    pub messages: Vec<Message>,
    /// Set on the last row of a playbook step: its final text. A resumed run skips every key
    /// that has one and rebuilds the conversation of the key that does not.
    pub output: Option<String>,
    pub usage: Usage,
    pub tainted: bool,
    /// The output stopped at the model's output limit: a resumed run retries it, as the live
    /// run would have.
    #[serde(default)]
    pub truncated: bool,
    pub at_ms: i64,
}

impl StepRecord {
    /// The transcript view: assistant text and tool summaries of this step.
    pub fn view(&self) -> CrewStep {
        let mut lines: Vec<String> = Vec::new();
        for m in &self.messages {
            for part in &m.content {
                match (m.role, part) {
                    (Role::User, Part::Text { text }) => lines.push(format!("› {}", clip(text, 400))),
                    (Role::Assistant, Part::Text { text }) if !text.trim().is_empty() => lines.push(text.trim().to_string()),
                    (_, Part::ToolCall(c)) => lines.push(format!("▸ {}", call_line(c))),
                    (_, Part::ToolResult(r)) => {
                        let first = r.content.lines().next().unwrap_or("").trim();
                        let n = r.content.lines().count();
                        let mark = if r.is_error { "✗" } else { "↳" };
                        let more = if n > 1 { format!(" (+{} lines)", n - 1) } else { String::new() };
                        lines.push(format!("  {mark} {}{more}", clip(first, 120)));
                    }
                    _ => {}
                }
            }
        }
        CrewStep { step: self.step, role: self.role.clone(), action: self.action.clone(), text: lines.join("
"), at_ms: self.at_ms }
    }
}

/// `name first-string-arg`, one line.
pub(crate) fn call_line(c: &ToolCall) -> String {
    let first = c.args.as_object().and_then(|o| o.values().find_map(|v| v.as_str())).unwrap_or("");
    let first = clip(first.lines().next().unwrap_or(""), 80);
    if first.is_empty() {
        c.name.clone()
    } else {
        format!("{} {first}", c.name)
    }
}

/// At most `max` chars, with an ellipsis when cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}
