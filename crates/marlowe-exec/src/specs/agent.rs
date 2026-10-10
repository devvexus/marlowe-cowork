//! The agent group (track F): todo, ask_user, skill, agent, send_message, exit_plan,
//! enter_worktree and exit_worktree. The tools' logic lives in `marlowe-agent` (`todo`,
//! `commands`, `subagent`, `worktree`); session state comes from `CallCtx::scope` (the
//! conversation's extensions, the daemon's `Questions`, `Subagents` and `Worktrees` services).
//! Loop-side behaviour (todo reminders, the stop check, lazy instructions) is middleware the
//! daemon adds per session (`marlowe-daemon/src/conductor_ops.rs`).
//!
//! The group is live on hosts the binary configures (the conductor's): `configure` turns it on,
//! so crew sandboxes and bare hosts do not offer these tools.

use std::path::PathBuf;

use marlowe_agent::{Consequence, Effects, ToolCallReq, ToolGroup};
use serde_json::json;

use super::{spec, ToolDef};
use crate::BuiltinTools;

/// Whether the agent group is live on a host, and the profile home it was configured with.
#[derive(Debug, Default)]
pub struct AgentGroup {
    pub enabled: bool,
    pub home: Option<PathBuf>,
}

fn live(t: &BuiltinTools) -> bool {
    t.state::<AgentGroup>().enabled
}

fn todo_spec() -> marlowe_model::ToolSpec {
    spec(
        "todo",
        "Keep a todo list for multi-step work: send the whole list each time. Keep exactly one item in_progress and \
         mark items completed as soon as they are done. Use it for tasks with three or more steps or when the user \
         gives several things to do; skip it for a single quick step.",
        json!({
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "What to do, imperative (\"Run the tests\")."},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                        "active_form": {"type": "string", "description": "Present tense, shown while in progress (\"Running the tests\")."}
                    },
                    "required": ["content", "status"],
                    "additionalProperties": false
                }
            }
        }),
        &["items"],
    )
}

fn ask_user_spec() -> marlowe_model::ToolSpec {
    spec(
        "ask_user",
        "Ask the user 1-4 multiple-choice questions and wait for the answers. Only when you are blocked on a decision \
         that only the user can make; otherwise choose sensibly and say so. The user can always answer in their own words.",
        json!({
            "questions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "header": {"type": "string", "description": "A label of at most 12 characters."},
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {"label": {"type": "string"}, "description": {"type": "string"}},
                                "required": ["label"],
                                "additionalProperties": false
                            }
                        },
                        "multi_select": {"type": "boolean"}
                    },
                    "required": ["question", "options"],
                    "additionalProperties": false
                }
            }
        }),
        &["questions"],
    )
}

fn skill_spec() -> marlowe_model::ToolSpec {
    spec(
        "skill",
        "Load a skill from the skills index: its instructions come back for you to follow (some run in a subagent and \
         return a result). Load the skill before starting work it covers.",
        json!({
            "name": {"type": "string", "description": "The skill's name as listed."},
            "args": {"type": "string", "description": "Arguments for the skill, if it takes any."}
        }),
        &["name"],
    )
}

fn agent_spec() -> marlowe_model::ToolSpec {
    spec(
        "agent",
        "Run a subagent on one task in a clean context and get its report back. Types: `explore` (fast read-only code \
         search), `plan` (read-only implementation plan), `general` (the full toolset), and any listed in the agents \
         directory. The agent cannot see this conversation: put everything it needs in `prompt`. Independent agents \
         called in the same step run in parallel. `background: true` returns at once and you are told when it \
         finishes. `isolation: \"worktree\"` runs it in its own git worktree.",
        json!({
            "type": {"type": "string", "description": "Agent type, e.g. explore, plan, general."},
            "description": {"type": "string", "description": "Three to five words naming the task."},
            "prompt": {"type": "string", "description": "The complete task and what to report back."},
            "isolation": {"type": "string", "enum": ["worktree"]},
            "background": {"type": "boolean"},
            "model": {"type": "string", "description": "Override the type's model: sonnet, opus, haiku, inherit or a model id."}
        }),
        &["type", "description", "prompt"],
    )
}

fn send_message_spec() -> marlowe_model::ToolSpec {
    spec(
        "send_message",
        "Continue a finished subagent (by the agent_id its report gave) with a follow-up; it keeps its earlier context.",
        json!({
            "agent_id": {"type": "string"},
            "text": {"type": "string"}
        }),
        &["agent_id", "text"],
    )
}

fn exit_plan_spec() -> marlowe_model::ToolSpec {
    spec(
        "exit_plan",
        "In plan mode, present the finished plan and ask the user to approve leaving plan mode to carry it out.",
        json!({"plan": {"type": "string", "description": "The steps and the files they change, in markdown."}}),
        &["plan"],
    )
}

fn enter_worktree_spec() -> marlowe_model::ToolSpec {
    spec(
        "enter_worktree",
        "Move this session into a new git worktree on its own branch, so changes stay off the current checkout.",
        json!({"name": {"type": "string", "description": "Short name for the branch and directory."}}),
        &[],
    )
}

fn exit_worktree_spec() -> marlowe_model::ToolSpec {
    spec(
        "exit_worktree",
        "Return to the original workspace. The branch is kept; `keep: true` also keeps the worktree directory.",
        json!({"keep": {"type": "boolean"}}),
        &[],
    )
}

fn describe_agent(call: &ToolCallReq) -> String {
    let ty = call.args.get("type").and_then(|v| v.as_str()).unwrap_or("general");
    let d = call.args.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let bg = if call.args.get("background").and_then(|v| v.as_bool()).unwrap_or(false) { " (background)" } else { "" };
    format!("agent {ty}: {}{bg}", crate::first_line(d, 60))
}

fn describe_todo(call: &ToolCallReq) -> String {
    let items = call.args.get("items").or_else(|| call.args.get("todos")).and_then(|v| v.as_array());
    match items {
        Some(a) => format!("todo {} items", a.len()),
        None => "todo".into(),
    }
}

fn agent_taints(t: &BuiltinTools, call: &ToolCallReq) -> bool {
    let ty = call.args.get("type").and_then(|v| v.as_str()).unwrap_or("general");
    marlowe_agent::subagent::may_taint(ty, t.workspace())
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(todo_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::todo::todo_call(a, cx))
            .describe(describe_todo)
            .enabled(live),
        ToolDef::new(ask_user_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::todo::ask_user_call(a, cx))
            .describe(|c| super::describe_by(c, ""))
            .enabled(live),
        ToolDef::new(skill_spec(), ToolGroup::Agent, Effects::READ, |_, a, cx| marlowe_agent::commands::skill_call(a, cx))
            .describe(|c| super::describe_by(c, "name"))
            .enabled(live),
        ToolDef::new(agent_spec(), ToolGroup::Agent, Effects::RUN, |_, a, cx| marlowe_agent::subagent::agent_call(a, cx))
            .describe(describe_agent)
            .taints(agent_taints)
            .enabled(live),
        ToolDef::new(send_message_spec(), ToolGroup::Agent, Effects::RUN, |_, a, cx| marlowe_agent::subagent::send_message_call(a, cx))
            .describe(|c| super::describe_by(c, "agent_id"))
            .taints(|t, _| marlowe_agent::subagent::may_taint("general", t.workspace()))
            .enabled(live),
        ToolDef::new(exit_plan_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::todo::exit_plan_call(a, cx))
            .consequence(|_, c, _| {
                let first = c.args.get("plan").and_then(|v| v.as_str()).map(|p| crate::first_line(p, 80)).unwrap_or_default();
                Consequence::Ask { summary: format!("leave plan mode and carry out the plan: {first}") }
            })
            .enabled(live),
        ToolDef::new(enter_worktree_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::worktree::enter_call(a, cx))
            .describe(|c| super::describe_by(c, "name"))
            .enabled(live),
        ToolDef::new(exit_worktree_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::worktree::exit_call(a, cx))
            .enabled(live),
    ]
}

/// Loop middleware for this group: none on the host; the conductor's comes from the daemon.
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// `persona/blocks/coding.md`: appended while the group is exposed and live.
pub fn persona_block() -> Option<&'static str> {
    Some(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/coding.md")))
}

/// Turns the group on for a host the binary builds (the conductor's).
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(AgentGroup { enabled: true, home: Some(setup.home.to_path_buf()) })
}

/// A core group: local models always get it, so nothing to detect.
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use marlowe_agent::{CallCtx, Consequence, SessionScope, ToolCtx, ToolHost};
    use marlowe_model::CancelToken;
    use marlowe_proto::SessionId;
    use serde_json::json;

    use crate::BuiltinTools;

    fn configured() -> BuiltinTools {
        super::super::configure(BuiltinTools::new(None), &super::super::HostSetup { home: Path::new("C:/home"), workspace: None })
    }

    #[test]
    fn the_group_is_live_only_on_configured_hosts() {
        let names = |h: &BuiltinTools| h.specs().into_iter().map(|s| s.name).collect::<Vec<_>>();
        let bare = names(&BuiltinTools::new(None));
        assert!(!bare.contains(&"todo".to_string()), "crew and bare hosts do not offer the agent group");
        let conf = names(&configured());
        for t in ["todo", "ask_user", "skill", "agent", "send_message", "exit_plan", "enter_worktree", "exit_worktree"] {
            assert!(conf.contains(&t.to_string()), "{t} missing from {conf:?}");
        }
        assert!(configured().persona_blocks().iter().any(|b| b.text().contains("<coding>")));
        assert!(BuiltinTools::new(None).persona_blocks().is_empty());
        for d in super::tools() {
            assert_eq!(d.spec.input_schema["additionalProperties"], json!(false), "{}", d.name());
        }
    }

    /// A15: the conductor's prefix for dusk at 32k (persona, its blocks, every exposed tool
    /// schema and the project sections at their caps) fits the 9k budget. Memory and crew tools
    /// are the daemon's and are counted at the integrator's gate on the wired binary.
    #[test]
    fn parity_a15_the_dusk_prefix_fits_nine_thousand_tokens_at_32k() {
        use marlowe_agent::context::{estimate_tokens, fit_prefix, tool_spec_tokens, PrefixBudget};
        use marlowe_agent::system::PromptSections;
        use marlowe_agent::{PersonaBlock, SystemPrompt};
        use marlowe_model::{ModelInfo, ProviderKind};

        let dusk = ModelInfo { context_window: Some(32_768), ..ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super") };
        let host = configured().with_model_exposure(&dusk);
        let tools = tool_spec_tokens(&host.specs());
        let persona = SystemPrompt::new().block(PersonaBlock::Memory).block(PersonaBlock::Crew).blocks_from(host.persona_blocks());
        let persona = estimate_tokens(&persona.persona_text());
        let budget = PrefixBudget::for_window(32_768);
        let sections = PromptSections {
            instructions: format!("<instructions>\n{}\n</instructions>", "Follow the project rules. ".repeat(400)),
            project_profile: "build: cargo build\ntest: cargo test\n".repeat(20),
            repo_map: "src/lib.rs: fn main, struct Agent\n".repeat(200),
            skills_index: "- skill: does a thing\n".repeat(40),
            output_style: String::new(),
            warnings: Vec::new(),
        };
        let fit = fit_prefix(persona, tools, &sections, budget);
        eprintln!("A15: prefix {} tokens = persona+blocks {persona} + tools {tools} + sections; budget {}", fit.tokens, budget.total);
        assert!(fit.tokens <= 9_000, "prefix {} tokens (persona {persona}, tools {tools}): {:?}", fit.tokens, fit.warnings);
        assert!(!fit.warnings.is_empty(), "the oversized sections were cut, visibly");
    }

    #[test]
    fn the_todo_tool_updates_session_state_and_exit_plan_asks() {
        let host = configured();
        let ctx = ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false);
        let cx = CallCtx::new(ctx, SessionScope::default());
        let call = marlowe_agent::ToolCallReq {
            id: "c".into(),
            name: "todo".into(),
            args: json!({"items": [{"content": "write it", "status": "in_progress"}, {"content": "test it", "status": "pending"}]}),
        };
        let out = host.call_scoped(&call, &cx);
        assert!(!out.is_error, "{}", out.content);
        let items = cx.scope.with_ext(|e| e.get::<marlowe_agent::todo::TodoState>().map(|t| t.items.len()));
        assert_eq!(items, Some(2));
        let plan = marlowe_agent::ToolCallReq { id: "p".into(), name: "exit_plan".into(), args: json!({"plan": "1. do it"}) };
        assert!(matches!(host.consequence(&plan), Consequence::Ask { .. }));
        let ask = marlowe_agent::ToolCallReq { id: "q".into(), name: "ask_user".into(), args: json!({"questions": [{"question": "Which?", "options": [{"label": "a"}]}]}) };
        let out = host.call_scoped(&ask, &cx);
        assert!(out.is_error && out.content.contains("No one can answer"), "headless: refused with guidance");
    }
}
