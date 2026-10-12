//! The agent group (track F): todo, ask_user, skill, agent, send_message, exit_plan,
//! enter_worktree and exit_worktree. The tools' logic lives in `marlowe-agent` (`todo`,
//! `commands`, `subagent`, `worktree`); session state comes from `CallCtx::scope` (the
//! conversation's extensions, the daemon's `Questions`, `Subagents` and `Worktrees` services).
//! Loop-side behaviour (todo reminders, the stop check, lazy instructions) is middleware the
//! daemon adds per session (`marlowe-daemon/src/conductor_ops.rs`).
//!
//! The group is live on hosts the binary configures (the conductor's): `configure` turns it on,
//! so crew sandboxes and bare hosts do not offer these tools.
//!
//! `skill` is declared as running, like `agent`: a skill may run pre-exec commands or fork an
//! agent with the caller's tools, so plan mode refuses it. Per call, the permission gate may
//! allow one that only reads (`commands::skill_read_only`, `subagent::read_only_type`).

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

/// Where this host's skills and agent types come from: the profile home and the host's root, as
/// the daemon's session resolves them.
fn sources(t: &BuiltinTools) -> marlowe_agent::instructions::Sources {
    marlowe_agent::instructions::Sources::for_session(t.state::<AgentGroup>().home.as_deref(), t.workspace())
}

fn todo_spec() -> marlowe_model::ToolSpec {
    spec(
        "todo",
        "Todo list for work of three or more steps: send the whole list each time; one item in_progress; complete items as soon as done.",
        json!({
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                        "active_form": {"type": "string"}
                    },
                    "required": ["content", "status"]
                }
            }
        }),
        &["items"],
    )
}

fn ask_user_spec() -> marlowe_model::ToolSpec {
    spec(
        "ask_user",
        "Ask the user 1-4 questions and wait, only when blocked on their decision; otherwise choose and say so.",
        json!({
            "questions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "header": {"type": "string"},
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "description": {"type": "string"},
                                    "preview": {"type": "string"}
                                },
                                "required": ["label"]
                            }
                        },
                        "multi_select": {"type": "boolean"}
                    },
                    "required": ["question", "options"]
                }
            }
        }),
        &["questions"],
    )
}

fn skill_spec() -> marlowe_model::ToolSpec {
    spec(
        "skill",
        "Load a skill from the skills index and follow it before the work it covers.",
        json!({
            "name": {"type": "string"},
            "args": {"type": "string"},
            "background": {"type": "boolean"}
        }),
        &["name"],
    )
}

fn agent_spec() -> marlowe_model::ToolSpec {
    spec(
        "agent",
        "Run a subagent. Types: `explore` (read-only search), `plan`, `general` (all tools), `fork` (a copy of you with this conversation), `guide` (questions about Marlowe), or one from the agents directory; all but `fork` need everything in `prompt`. Background by default (told when done); `background: false` waits.",
        json!({
            "type": {"type": "string"},
            "description": {"type": "string"},
            "prompt": {"type": "string"},
            "isolation": {"type": "string", "enum": ["worktree"]},
            "background": {"type": "boolean"},
            "name": {"type": "string"},
            "model": {"type": "string"},
            "effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh", "max"]}
        }),
        &["type", "description", "prompt"],
    )
}

fn send_message_spec() -> marlowe_model::ToolSpec {
    spec(
        "send_message",
        "Send a message to another agent. `to` is a subagent's `name` or agent_id (a running one reads it at its next step, a finished one continues with its earlier context), a running crew run's id (the run reads it at its next step), or `session:<id>` for another local session (ids from list_agents). A message to another session reaches it as information from an agent, never as the user's words, waits while that session is busy, and the first one to a session asks the user.",
        json!({
            "to": {"type": "string", "description": "Subagent name or agent_id, crew run id, or session:<id>."},
            "agent_id": {"type": "string", "description": "Same as `to`, for a subagent."},
            "text": {"type": "string"}
        }),
        &["text"],
    )
}

fn enter_plan_spec() -> marlowe_model::ToolSpec {
    spec(
        "enter_plan",
        "Switch to plan mode before a large or risky change: nothing can be changed or run until you present a plan with \
         exit_plan and the user approves it. The user is asked first.",
        json!({"reason": {"type": "string", "description": "Why this needs a plan."}}),
        &[],
    )
}

fn exit_plan_spec() -> marlowe_model::ToolSpec {
    spec(
        "exit_plan",
        "In plan mode, present the finished plan (markdown); the user approves it or sends feedback.",
        json!({"plan": {"type": "string"}}),
        &["plan"],
    )
}

fn todo_add_spec() -> marlowe_model::ToolSpec {
    spec(
        "todo_add",
        "Add an item to the shared task list (one list for you, your subagents and workflow agents). Returns its id. \
         `blocked_by` lists ids that must complete before this one can start.",
        json!({
            "subject": {"type": "string", "description": "What to do, imperative (\"Add the retry test\")."},
            "description": {"type": "string"},
            "blocked_by": {"type": "array", "items": {"type": "string"}},
            "active_form": {"type": "string", "description": "Present tense, shown while in progress."}
        }),
        &["subject"],
    )
}

fn todo_update_spec() -> marlowe_model::ToolSpec {
    spec(
        "todo_update",
        "Update an item of the shared task list by id: status (an item cannot start while an item it is blocked by is \
         open), owner, subject, description or blockers. Mark an item completed as soon as it is done.",
        json!({
            "id": {"type": "string"},
            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
            "owner": {"type": "string"},
            "subject": {"type": "string"},
            "description": {"type": "string"},
            "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Replaces the blockers."},
            "add_blocked_by": {"type": "array", "items": {"type": "string"}}
        }),
        &["id"],
    )
}

fn todo_get_spec() -> marlowe_model::ToolSpec {
    spec(
        "todo_get",
        "One item of the shared task list in full (status, owner, description, what blocks it and what it blocks), or the \
         whole list without an id.",
        json!({"id": {"type": "string"}}),
        &[],
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
        "Return to the original workspace; the branch is kept. `keep: true` keeps the directory too. Uncommitted \
         changes block removal unless `discard_changes: true` (asks).",
        json!({"keep": {"type": "boolean"}, "discard_changes": {"type": "boolean"}}),
        &[],
    )
}

fn describe_agent(call: &ToolCallReq) -> String {
    let ty = call.args.get("type").and_then(|v| v.as_str()).unwrap_or("general");
    let d = call.args.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let bg = if call.args.get("background").and_then(|v| v.as_bool()).unwrap_or(true) { " (background)" } else { "" };
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
    marlowe_agent::subagent::may_taint(ty, t.state::<AgentGroup>().home.as_deref(), t.workspace())
}

/// A forked skill runs an agent: it taints as that agent type would.
fn skill_taints(t: &BuiltinTools, call: &ToolCallReq) -> bool {
    marlowe_agent::commands::skill_fork_agent(&call.args, &sources(t))
        .is_some_and(|ty| marlowe_agent::subagent::may_taint(&ty, t.state::<AgentGroup>().home.as_deref(), t.workspace()))
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(todo_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::todo::todo_call(a, cx))
            .describe(describe_todo)
            .enabled(live),
        ToolDef::new(todo_add_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::tasklist::todo_add_call(a, cx))
            .describe(|c| super::describe_by(c, "subject"))
            .enabled(live),
        ToolDef::new(todo_update_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::tasklist::todo_update_call(a, cx))
            .describe(|c| super::describe_by(c, "id"))
            .enabled(live),
        ToolDef::new(todo_get_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::tasklist::todo_get_call(a, cx))
            .describe(|c| super::describe_by(c, "id"))
            .enabled(live),
        ToolDef::new(ask_user_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::todo::ask_user_call(a, cx))
            .describe(|c| super::describe_by(c, ""))
            .enabled(live),
        ToolDef::new(skill_spec(), ToolGroup::Agent, Effects::RUN, |t, a, cx| {
            marlowe_agent::commands::skill_call_gated(a, cx, &sources(t))
        })
        .describe(|c| super::describe_by(c, "name"))
        .consequence(|t, c, _| marlowe_agent::commands::skill_consequence(&c.args, &sources(t)))
        .taints(skill_taints)
        .enabled(live),
        ToolDef::new(agent_spec(), ToolGroup::Agent, Effects::RUN, |_, a, cx| marlowe_agent::subagent::agent_call(a, cx))
            .describe(describe_agent)
            .taints(agent_taints)
            .enabled(live),
        ToolDef::new(send_message_spec(), ToolGroup::Agent, Effects::RUN, |_, a, cx| marlowe_agent::subagent::send_message_call(a, cx))
            .describe(|c| super::describe_by(c, "agent_id"))
            .taints(|t, _| marlowe_agent::subagent::may_taint("general", t.state::<AgentGroup>().home.as_deref(), t.workspace()))
            .enabled(live),
        ToolDef::new(enter_plan_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::planfile::enter_plan_call(a, cx))
            .consequence(|_, c, _| {
                let why = c.args.get("reason").and_then(|v| v.as_str()).map(|r| crate::first_line(r, 80)).unwrap_or_default();
                Consequence::Ask { summary: format!("enter plan mode (read-only until a plan is approved): {why}") }
            })
            .enabled(live),
        // The plan is approved in the plan modal (a file the user may edit), not by a generic approval.
        ToolDef::new(exit_plan_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::planfile::exit_plan_call(a, cx))
            .describe(|c| super::describe_by(c, "plan"))
            .enabled(live),
        ToolDef::new(enter_worktree_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::worktree::enter_call(a, cx))
            .describe(|c| super::describe_by(c, "name"))
            .enabled(live),
        ToolDef::new(exit_worktree_spec(), ToolGroup::Agent, Effects::NONE, |_, a, cx| marlowe_agent::worktree::exit_call(a, cx))
            .consequence(|_, c, _| marlowe_agent::worktree::exit_consequence(c))
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
        assert!(!BuiltinTools::new(None).persona_blocks().iter().any(|b| b.text().contains("<coding>")));
        for d in super::tools() {
            assert_eq!(d.spec.input_schema["additionalProperties"], json!(false), "{}", d.name());
        }
    }

    /// A15, the dusk profile at 32k, this host alone: persona and blocks (v3, memory, crew and the
    /// live groups') inside their 2,000-token slice, the built-in tool schemas inside their 7,000,
    /// and the fitter keeps the whole prefix inside 12,500 with every oversized section cut,
    /// visibly. Every built-in is listed (nothing is behind `tool_search`), so this waits for the
    /// compact specs; the wired conductor's own tools are counted in
    /// `parity_a15_instructions_at_their_cap_reach_dusk_uncut` and in the daemon's
    /// `the_wired_dusk_prefix_fits_its_slices_in_a_repository`.
    #[test]
    #[ignore = "until the compact specs land (Specs stage)"]
    fn the_fitter_keeps_the_dusk_prefix_inside_its_budget() {
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
        assert!(persona <= budget.persona, "persona and blocks are {persona} tokens, over their {} slice", budget.persona);
        assert!(tools <= budget.tools, "the built-ins are {tools} tokens, over their {} slice", budget.tools);
        assert!(fit.tokens <= budget.total, "prefix {} tokens (persona {persona}, tools {tools}): {:?}", fit.tokens, fit.warnings);
        assert!(!fit.warnings.is_empty(), "the oversized sections were cut, visibly");
    }

    /// Nothing hides behind a search: a 32k window lists the same tools as a 200k one, and there
    /// is no `tool_search` (that survives only for MCP tools, in the daemon's MCP host).
    #[test]
    fn every_built_in_is_listed_on_a_small_window() {
        use marlowe_model::{ModelInfo, ProviderKind};

        let dusk = ModelInfo { context_window: Some(32_768), ..ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super") };
        let small = configured().with_model_exposure(&dusk);
        let names: Vec<String> = small.specs().into_iter().map(|s| s.name).collect();
        for t in ["todo_add", "todo_update", "todo_get", "enter_plan", "send_message", "enter_worktree", "exit_worktree", "web_fetch", "workflow"] {
            assert!(names.contains(&t.to_string()), "{t} is not listed at 32k: {names:?}");
        }
        assert!(!names.contains(&"tool_search".to_string()), "no search for built-ins");
        let wide = ModelInfo { context_window: Some(200_000), ..ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super") };
        let big: Vec<String> = configured().with_model_exposure(&wide).specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, big, "the window never decides which tools are listed");
    }

    /// A15: project instructions at their 1.5k cap reach dusk whole at 32k, with this host's
    /// tools and the conductor's memory and dispatch tools (fixtures below) both inside their
    /// slices, so the fitter has nothing to take from the project sections.
    #[test]
    #[ignore = "until the compact specs land (Specs stage)"]
    fn parity_a15_instructions_at_their_cap_reach_dusk_uncut() {
        use marlowe_agent::context::{estimate_tokens, fit_prefix, tool_spec_tokens, PrefixBudget};
        use marlowe_agent::system::PromptSections;
        use marlowe_agent::{PersonaBlock, SystemPrompt};
        use marlowe_model::{ModelInfo, ProviderKind};

        let dusk = ModelInfo { context_window: Some(32_768), ..ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super") };
        let host = configured().with_model_exposure(&dusk);
        let mut specs = host.specs();
        specs.extend(wired_conductor_extras());
        let tools = tool_spec_tokens(&specs);
        let persona = SystemPrompt::new().block(PersonaBlock::Memory).block(PersonaBlock::Crew).blocks_from(host.persona_blocks());
        let persona = estimate_tokens(&persona.persona_text());
        // About 1.5k tokens of instructions (chars/3), the per-section cap.
        let instructions = format!("<instructions>\n{}\n</instructions>", "Follow the project rules. ".repeat(170));
        let sections = PromptSections { instructions: instructions.clone(), ..Default::default() };
        let budget = PrefixBudget::for_window(32_768);
        let fit = fit_prefix(persona, tools, &sections, budget);
        assert!(persona <= budget.persona && tools <= budget.tools, "persona {persona} / {}, tools {tools} / {}", budget.persona, budget.tools);
        assert_eq!(fit.sections.instructions, instructions, "instructions were cut (persona {persona}, tools {tools}): {:?}", fit.warnings);
    }

    /// The tools the wired conductor adds to this host (the daemon's memory tools, crew
    /// dispatch), as fixtures of their schemas (`marlowe-daemon/src/mind.rs`,
    /// `marlowe-crew/src/dispatch.rs`, abridged only where marked). MCP tools are not counted.
    fn wired_conductor_extras() -> Vec<marlowe_model::ToolSpec> {
        let t = |name: &str, description: &str, schema: serde_json::Value| marlowe_model::ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema: schema,
        };
        let s = |d: &str| json!({"type": "string", "description": d});
        vec![
            t(
                "remember",
                "Record something durable about the user, once, when they state it, unannounced; not one-offs. A decision needs its `reason`. Results and notes are retrieved when relevant; the rest goes in the profile. To change a record, use correct.",
                json!({"type": "object", "properties": {
                    "text": s("One self-contained statement."),
                    "kind": {"type": "string", "enum": ["preference", "decision", "fact", "procedure", "result", "note"]},
                    "reason": {"type": "string"},
                    "scope": {"type": "string", "enum": ["global", "project"], "default": "global"}},
                    "required": ["text", "kind"]}),
            ),
            t(
                "recall",
                "Search stored memories (past conversations, episodes, anything not in the profile) when the user asks about the past.",
                json!({"type": "object", "properties": {"query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 5}}, "required": ["query"]}),
            ),
            t(
                "forget",
                "Forget a memory by id. The audit journal keeps the record, not the text.",
                json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ),
            t(
                "correct",
                "Replace a memory's text (recall finds the id); use it, not remember, for something recorded that changed. A decision keeps its reason only if the text ends with \"(reason: ...)\". Refused if it would lower trust.",
                json!({"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}},
                    "required": ["id", "text"]}),
            ),
            t(
                "dispatch",
                "Send long, parallel or bulky work to a crew agent that runs in the background in its own sandbox. You get \
                 its run id at once; the result arrives later in this conversation. The crew cannot see this conversation, \
                 so the goal must carry the context it needs.",
                // Abridged: the role menu lists the profile's roles (three here); the daemon's
                // `the_wired_dusk_prefix_fits_its_slices_in_a_repository` counts the real menu.
                json!({"type": "object", "properties": {
                    "role": {"type": "string", "enum": ["researcher", "engineer", "reviewer"],
                             "description": "Who does the work. researcher: literature and evidence · engineer: builds in a \
                              worktree · reviewer: adversarial review"},
                    "goal": s("What to find out or build, with the paths, constraints and definition of done the crew needs."),
                    "playbook": {"type": "string", "enum": ["deep_research", "build"], "description": "deep_research: \
                     plan, parallel scouts, adversarial verify, cited report. build: plan, implement in a worktree, test \
                     loop, review, diff for merge. Omit for a single role run."},
                    "workspace": s("Default: this session's workspace."),
                    "budget": {"type": "object", "properties": {"tokens": {"type": "integer"}, "steps": {"type": "integer"},
                             "usd": {"type": "number"}}}},
                    "required": ["role", "goal"]}),
            ),
        ]
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("marlowe-exec-agent-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A13 and C12 on the real built-ins: `BuiltinTools` resolves paths against the root it was
    /// built with, so the loop's step host must rebuild it for the worktree and the added root.
    #[test]
    fn parity_a13_c12_the_real_file_tools_follow_the_worktree_and_added_roots() {
        use std::sync::Arc;

        use marlowe_agent::instructions::WorkspaceRoots;
        use marlowe_agent::worktree::{step_host, WorkspaceHosts, WorktreeHandle, WorktreeState};
        use marlowe_agent::{Conversation, Services};

        let root = tmp("a13c12");
        let (repo, wt, other) = (root.join("repo"), root.join("wt"), root.join("other"));
        for d in [&repo, &wt, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let base: Arc<dyn ToolHost> = Arc::new(BuiltinTools::new(Some(repo.clone())));
        let services = Services::new()
            .with(WorkspaceHosts::new(|r: &Path| Arc::new(BuiltinTools::new(Some(r.to_path_buf()))) as Arc<dyn ToolHost>));
        let write_call = |path: &str, content: &str| marlowe_agent::ToolCallReq {
            id: "w".into(),
            name: "write".into(),
            args: json!({"path": path, "content": content}),
        };
        let cx = |ws: &Path| CallCtx::new(ToolCtx::new(SessionId("s".into()), 0, Some(ws.to_path_buf()), CancelToken::new(), false), SessionScope::default());

        // The session moved into the worktree (what WorktreeMiddleware leaves behind).
        let mut convo = Conversation::new(SessionId("s".into()), Some(wt.clone()));
        let handle = WorktreeHandle { id: "feat".into(), root: wt.clone(), branch: "marlowe/feat".into(), base: "abc".into() };
        convo.ext.insert(WorktreeState { original: Some(repo.clone()), handle: Some(handle), switch_to: None });
        // Without the step host, the session's tools still write into the main checkout.
        let out = base.call_scoped(&write_call("stale.txt", "x"), &cx(&wt));
        assert!(!out.is_error, "{}", out.content);
        assert!(repo.join("stale.txt").is_file() && !wt.join("stale.txt").exists(), "the defect the step host fixes");
        let host = step_host(&base, &services, &convo);
        let out = host.call_scoped(&write_call("notes.txt", "IN-WORKTREE"), &cx(&wt));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(wt.join("notes.txt")).unwrap(), "IN-WORKTREE");
        assert!(!repo.join("notes.txt").exists(), "the main checkout is untouched");
        assert_eq!(host.consequence(&write_call("more.txt", "y")), Consequence::Free, "a worktree write is a workspace write");

        // An added root: an absolute write under it is free and lands there; elsewhere still asks.
        let mut convo = Conversation::new(SessionId("s".into()), Some(repo.clone()));
        let mut roots = WorkspaceRoots::default();
        roots.add(&other);
        convo.ext.insert(roots);
        let target = other.join("src").join("x.txt");
        let call = write_call(&target.display().to_string(), "ADDED");
        assert!(matches!(base.consequence(&call), Consequence::Ask { .. }), "outside the session's own root it asks");
        let host = step_host(&base, &services, &convo);
        assert_eq!(host.consequence(&call), Consequence::Free, "under an added root it does not");
        let out = host.call_scoped(&call, &cx(&repo));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ADDED");
        let elsewhere = write_call(&root.join("elsewhere.txt").display().to_string(), "NO");
        assert!(matches!(host.consequence(&elsewhere), Consequence::Ask { .. }));
    }

    /// X11: the `skill` tool asks before a skill whose template runs commands; a forked skill
    /// taints as its agent type does, and agent types resolve with the profile home's.
    #[test]
    fn the_skill_tool_asks_before_pre_exec_and_taints_like_its_agent() {
        let root = tmp("skill");
        let (home, repo) = (root.join("home"), root.join("repo"));
        write(&repo.join(".claude/skills/status/SKILL.md"), "---\nname: status\ndescription: d\n---\nState: !`git status`");
        write(&repo.join(".claude/skills/plain/SKILL.md"), "---\nname: plain\ndescription: d\n---\nJust text.");
        write(&repo.join(".claude/skills/research/SKILL.md"), "---\nname: research\ndescription: d\ncontext: fork\nagent: explore\n---\nLook it up.");
        // A user-level `explore` that reaches the web overrides the read-only built-in.
        write(&home.join("agents/explore.md"), "---\nname: explore\ndescription: web explorer\ntools: Read, WebFetch\n---\nExplore.");
        let host = super::super::configure(BuiltinTools::new(Some(repo.clone())), &super::super::HostSetup { home: &home, workspace: Some(&repo) });
        let req = |name: &str, args: serde_json::Value| marlowe_agent::ToolCallReq { id: "k".into(), name: name.into(), args };
        match host.consequence(&req("skill", json!({"name": "status"}))) {
            Consequence::Ask { summary } | Consequence::AskEvery { summary } => assert!(summary.contains("git status"), "{summary}"),
            Consequence::Free => panic!("a skill that runs commands must ask"),
        }
        assert_eq!(host.consequence(&req("skill", json!({"name": "plain", "args": "!`rm -rf x`"}))), Consequence::Free);
        assert!(host.taints(&req("skill", json!({"name": "research"}))), "the fork runs the web-reaching explore");
        assert!(!host.taints(&req("skill", json!({"name": "plain"}))));
        assert!(host.taints(&req("agent", json!({"type": "explore"}))), "the home's explore overrides the built-in");
        // Plan mode keys on effects: `skill` runs (track E's rule refuses it), like `agent`.
        let m = host.manifest("skill").expect("skill manifest");
        assert!(m.effects.runs && m.effects.writes, "a skill can fork a writing agent or run commands");
        assert!(matches!(host.consequence(&req("exit_worktree", json!({"discard_changes": true}))), Consequence::Ask { .. }));
        assert_eq!(host.consequence(&req("exit_worktree", json!({}))), Consequence::Free);
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
        // The plan is approved in the plan modal, not by a generic approval; entering plan mode asks.
        assert_eq!(host.consequence(&plan), Consequence::Free);
        let enter = marlowe_agent::ToolCallReq { id: "e".into(), name: "enter_plan".into(), args: json!({"reason": "risky"}) };
        assert!(matches!(host.consequence(&enter), Consequence::Ask { .. }));
        let ask = marlowe_agent::ToolCallReq { id: "q".into(), name: "ask_user".into(), args: json!({"questions": [{"question": "Which?", "options": [{"label": "a"}]}]}) };
        let out = host.call_scoped(&ask, &cx);
        assert!(out.is_error && out.content.contains("No one can answer"), "headless: refused with guidance");
    }
}
