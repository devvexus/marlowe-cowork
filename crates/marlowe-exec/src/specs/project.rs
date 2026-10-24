//! The `manage_project` tool: how the user-facing model starts and steers a project (design:
//! `docs/research/projects-design.md`, section 6).
//!
//! The manager only designs: it writes the plan, the service parks the project on its plan card
//! and returns at once (non-negotiable 4), and every later step (approval, checkin answers,
//! cancel, reopen) is one short call. The tool belongs to the workflow group, beside `team`, and
//! shares its host flag: only the conductor has it. The service itself ([`ProjectService`]) is
//! the daemon's; a host without one answers every call with a plain sentence.
//!
//! Two guards live here and not in the service, because they are about the conversation:
//!
//! - A plan cannot be approved in the turn that created it: the user has not seen the card yet.
//!   (After a restart the guard has forgotten the project, which is fine: the card was shown.)
//! - Once untrusted content has entered the conversation, an answer that sends the project on
//!   (`approve`, `keep_researching`, `steer`) asks the user, so a page cannot approve its own
//!   plan.
//!
//! Nothing a project read reaches this tool's results as free text: `status` and `list` are
//! built by code from counts, phases and card options; the result itself arrives as a relayed
//! notice that is marked tainted.

use std::collections::HashMap;
use std::sync::Mutex;

use marlowe_agent::{CallCtx, Consequence, Effects, Exposure, ToolCallReq, ToolGroup, ToolOutcome};
use marlowe_project::{ProjectError, ProjectService};
use serde_json::Value;

use super::ToolDef;
use crate::BuiltinTools;

/// What the tool remembers across calls on a host.
#[derive(Debug, Default)]
pub struct ProjectGroup {
    /// Projects created in this process: the session and the turn that made them.
    created: Mutex<HashMap<String, (String, u32)>>,
}

fn group(t: &BuiltinTools) -> std::sync::Arc<ProjectGroup> {
    t.state::<ProjectGroup>()
}

fn spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("manage_project")
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

/// The actions, as the error for a missing or unknown one lists them.
const ACTIONS: &str = "create, revise, status, list, answer, checkin, cancel, reopen";

/// What models write for the card's answers, mapped to the answers (a few words a person says,
/// not a guess at meaning: anything else is refused with the list).
fn normal_answer(word: &str) -> String {
    let w = word.trim().to_lowercase().replace([' ', '-'], "_");
    match w.as_str() {
        "yes" | "ok" | "okay" | "go" | "go_ahead" | "approved" | "confirm" | "accept" | "proceed" => "approve".into(),
        "more" | "continue" | "keep_going" | "keep" | "research_more" | "keepresearching" => "keep_researching".into(),
        "stop" | "abort" | "no" | "reject" | "cancelled" | "canceled" => "cancel".into(),
        _ => w,
    }
}

/// `plan` as an object. A local model sometimes sends the object as a JSON string.
fn plan_arg(a: &Value) -> Result<Value, String> {
    match a.get("plan") {
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        Some(Value::String(t)) if t.trim_start().starts_with('{') => serde_json::from_str(t).map_err(|e| format!("`plan` must be a JSON object; the string given is not valid JSON ({e}).")),
        _ => Err(r#"plan: need an object like {"objective":"what the user wants and why, one or two sentences","questions":["one question to find out","another"],"fields":[{"key":"model_name"},{"key":"status","values":["released","announced","rumoured"]}]}."#.into()),
    }
}

fn fail(e: ProjectError) -> ToolOutcome {
    ToolOutcome::error_of(e.class(), e.to_string())
}

/// The id a call means: `id`, or the only unfinished project when `id` is left out.
fn project_id(svc: &ProjectService, a: &Value) -> Result<String, ToolOutcome> {
    if let Some(id) = s(a, "id") {
        return Ok(id.to_string());
    }
    let open: Vec<String> = svc.rows().unwrap_or_default().into_iter().filter(|r| !matches!(r.status.as_str(), "finished" | "cancelled" | "failed")).map(|r| r.id).collect();
    match open.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(ToolOutcome::error_of("project_missing", "id: there is no unfinished project; `list` shows all of them.")),
        _ => Err(ToolOutcome::error_of("project_missing", format!("id: which project? Unfinished: {}. Pass one as `id`.", open.join(", ")))),
    }
}

fn call(t: &BuiltinTools, a: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(svc) = cx.scope.services.get::<ProjectService>() else {
        return ToolOutcome::error("projects run in the daemon; this session has no project service. Do the work yourself or use a team task.");
    };
    let Some(action) = s(a, "action") else {
        return ToolOutcome::error_of("project_action", format!("action: say what to do: {ACTIONS}."));
    };
    match action {
        "create" => {
            let plan = match plan_arg(a) {
                Ok(p) => p,
                Err(e) => return ToolOutcome::error_of("project_plan", e),
            };
            match svc.create(Some(&cx.session.0), cx.workspace.as_deref(), &plan) {
                Ok(made) => {
                    group(t).created.lock().unwrap_or_else(|e| e.into_inner()).insert(made.id.clone(), (cx.session.0.clone(), cx.turn));
                    ToolOutcome::ok(
                        format!(
                            "{}\n\nThe project is made and waiting for the user. Tell them in two or three lines what it will look into and ask whether to go ahead. \
                             When they agree, call manage_project{{action:\"answer\", id:\"{}\", answer:\"approve\"}}. To change the plan first, call revise with only the keys that change.",
                            made.card, made.id
                        ),
                        format!("project {} created", made.id),
                    )
                }
                // A refused plan made nothing; a small model otherwise goes on to `revise` an id it invented.
                Err(e @ ProjectError::Plan(_)) => ToolOutcome::error_of(e.class(), format!("{e}\nNothing was created yet. Call manage_project create again with the plan fixed.")),
                Err(e) => fail(e),
            }
        }
        "revise" => {
            let id = match project_id(svc, a) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let changes = match plan_arg(a) {
                Ok(p) => p,
                Err(_) => return ToolOutcome::error_of("project_plan", r#"plan: give only the keys to change, e.g. {"questions":["the full new list of questions"]}."#),
            };
            match svc.revise(&id, &changes) {
                Ok(made) => ToolOutcome::ok(format!("{}\n\nThe plan is revised and still waiting for the user's approval.", made.card), format!("project {id} revised")),
                Err(e) => fail(e),
            }
        }
        "status" => match project_id(svc, a).and_then(|id| svc.status(&id).map_err(fail)) {
            Ok(text) => ToolOutcome::ok(text, "project status"),
            Err(e) => e,
        },
        "list" => match svc.list() {
            Ok(text) => ToolOutcome::ok(text, "projects"),
            Err(e) => fail(e),
        },
        "answer" => {
            let id = match project_id(svc, a) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let Some(choice) = s(a, "answer") else {
                return ToolOutcome::error_of("project_answer", "answer: say which: approve, keep_researching, steer (with the user's words in `text`) or cancel. `status` shows the card's options.");
            };
            let choice = normal_answer(choice);
            if choice == "approve" {
                if let Some((session, turn)) = group(t).created.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
                    if *session == cx.session.0 && *turn == cx.turn {
                        return ToolOutcome::error_of("project_state", format!("{id} was made in this turn, so the user has not seen its plan yet. Show them the questions, ask, and approve in your next turn."));
                    }
                }
            }
            match svc.answer(&id, s(a, "gate"), &choice, s(a, "text")) {
                Ok(done) => ToolOutcome::ok(done.line, format!("project {id}: {}", done.choice)),
                Err(e) => fail(e),
            }
        }
        "checkin" => match project_id(svc, a).and_then(|id| svc.checkin(&id).map_err(fail)) {
            Ok(text) => ToolOutcome::ok(text, "checkin requested"),
            Err(e) => e,
        },
        "cancel" => match project_id(svc, a).and_then(|id| svc.cancel(&id).map(|()| id).map_err(fail)) {
            Ok(id) => ToolOutcome::ok(format!("{id} is cancelled. It can be reopened from a stage."), "project cancelled"),
            Err(e) => e,
        },
        "reopen" => {
            let Some(phase) = s(a, "phase") else {
                return ToolOutcome::error_of("project_state", "phase: which stage to rerun from, e.g. DataValidation, or `final`.");
            };
            match project_id(svc, a).and_then(|id| svc.reopen(&id, phase).map_err(fail)) {
                Ok(line) => ToolOutcome::ok(line, "project reopened"),
                Err(e) => e,
            }
        }
        other => ToolOutcome::error_of("project_action", format!("action: `{other}` is not one of: {ACTIONS}.")),
    }
}

/// Answers that send a project on cost the user an approval once untrusted content is in the
/// conversation; everything else is free.
fn consequence(_t: &BuiltinTools, c: &ToolCallReq, exposure: &Exposure) -> Consequence {
    let a = &c.args;
    if s(a, "action") != Some("answer") || !exposure.tainted {
        return Consequence::Free;
    }
    let choice = s(a, "answer").map(normal_answer).unwrap_or_default();
    if matches!(choice.as_str(), "approve" | "keep_researching" | "steer") {
        let id = s(a, "id").unwrap_or("the project");
        return Consequence::Ask { summary: format!("{choice} {id}: it will read public web pages it finds") };
    }
    Consequence::Free
}

fn describe(c: &ToolCallReq) -> String {
    let a = &c.args;
    match (s(a, "action"), s(a, "id")) {
        (Some("answer"), Some(id)) => format!("project {id} {}", s(a, "answer").unwrap_or("answer")),
        (Some(act), Some(id)) => format!("project {act} {id}"),
        (Some(act), None) => format!("project {act}"),
        _ => "project".into(),
    }
}

/// The group's tools, appended to the workflow group's.
pub fn tools() -> Vec<ToolDef> {
    vec![ToolDef::new(spec(), ToolGroup::Workflow, Effects::RUN, call)
        .consequence(consequence)
        .describe(describe)
        // Same host as `team`: the conductor's.
        .enabled(|t| t.state::<super::workflow::WorkflowGroup>().conductor)]
}

/// Where a session's research turn stands with the route below.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The turn's message asks for research and no project was started yet.
    Pending,
    /// A web call was answered with the way to a project; the rest of this step's web calls
    /// get the same answer, the next step runs them.
    Bounced,
    Off,
}

/// Research goes to a project. The turn reminders say so; found live, dawn answered "Research the
/// three most popular Rust TUI libraries ..." with thirteen web searches of its own, each an ask.
/// In a session that has projects, the first web search, fetch or arXiv call of a turn whose
/// message asks for research is answered with the way to start one. A `manage_project` call ends
/// it, and the model may still search on at its next step (one quick fact needs no project).
pub struct ResearchRoute {
    sessions: Mutex<HashMap<String, Route>>,
}

/// What a web call in a research turn is answered with.
pub const RESEARCH_ROUTE: &str = "Not run: the user asked for research, and research is a project: start one with manage_project{action:\"create\"} (objective, questions, fields); its plan card is the plan the user approves, and agents do the reading. Search yourself only for one quick fact, by calling the tool again.";

impl marlowe_agent::ToolMiddleware for ResearchRoute {
    fn name(&self) -> &str {
        "research-route"
    }

    fn before_turn(&self, convo: &mut marlowe_agent::Conversation, text: &mut String) -> marlowe_agent::TurnFlow {
        let route = if marlowe_agent::teams::is_research_request(text) { Route::Pending } else { Route::Off };
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(convo.session.0.clone(), route);
        marlowe_agent::TurnFlow::Continue
    }

    fn before_approval(&self, call: &mut marlowe_model::ToolCall, cx: &CallCtx) -> marlowe_agent::CallFlow {
        let mut all = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(route) = all.get_mut(&cx.tool.session.0) else { return marlowe_agent::CallFlow::Continue };
        if call.name == "manage_project" {
            *route = Route::Off;
            return marlowe_agent::CallFlow::Continue;
        }
        let web = matches!(call.name.as_str(), "web_search" | "web_fetch" | "arxiv");
        if web && *route != Route::Off && cx.scope.services.get::<ProjectService>().is_some() {
            *route = Route::Bounced;
            return marlowe_agent::CallFlow::Done(ToolOutcome::error(RESEARCH_ROUTE));
        }
        marlowe_agent::CallFlow::Continue
    }

    fn after_step(&self, _info: marlowe_agent::StepInfo, convo: &mut marlowe_agent::Conversation) -> Vec<marlowe_model::Message> {
        let mut all = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(route) = all.get_mut(&convo.session.0).filter(|r| **r == Route::Bounced) {
            *route = Route::Off;
        }
        Vec::new()
    }
}

/// The route, for the conductor's host.
pub fn research_route() -> std::sync::Arc<dyn marlowe_agent::ToolMiddleware> {
    std::sync::Arc::new(ResearchRoute { sessions: Mutex::new(HashMap::new()) })
}

/// The persona block for projects: how the manager plans, asks and reports.
pub fn persona_block() -> &'static str {
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/project.md"))
}

#[cfg(test)]
mod tests;
