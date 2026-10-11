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
        "Return to the original workspace; the branch is kept. `keep: true` keeps the directory too. Uncommitted \
         changes block removal unless `discard_changes: true` (asks).",
        json!({"keep": {"type": "boolean"}, "discard_changes": {"type": "boolean"}}),
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

    /// The fitter keeps dusk's prefix at 32k inside the 9k budget by cutting the project sections,
    /// and says so. It is not A15 (that is `parity_a15_instructions_at_their_cap_reach_dusk_uncut`).
    /// Since wave 1 merged, the local core groups' schemas alone are about 5.6k tokens against their
    /// 3k slice, and the fitter cannot cut tools: built-ins must be deferred behind `tool_search`
    /// (T31) before this holds again. Track F3 re-measures A15 with them deferred.
    #[test]
    #[ignore = "core tool schemas (~5.6k tokens) overrun their 3k slice at 32k until built-ins defer behind tool_search (T31, F3)"]
    fn the_fitter_keeps_the_dusk_prefix_inside_nine_thousand_tokens() {
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

    /// A15: project instructions at their 1.5k cap reach dusk whole at 32k (counting this
    /// host's tools only; the daemon's test counts the wired conductor's). Fails today: the
    /// persona and its blocks (about 5.2k tokens, `persona/v3.md` alone about 4.2k) overrun
    /// their 2k slice and the fitter takes the difference from the project sections. Ignored
    /// until the persona is trimmed or the 9k budget re-sliced (Matthew's call). A15: failing.
    #[test]
    #[ignore = "A15 fails: persona + blocks overrun the 2k slice at 32k until trimmed or re-sliced"]
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
        let fit = fit_prefix(persona, tools, &sections, PrefixBudget::for_window(32_768));
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
                "Record something durable about the user. Preferences, decisions, facts and procedures go into the profile \
                 that is in context every turn. Call it once, when the user states it, without announcing it. Not for \
                 one-off requests or small talk. To change something already recorded, use correct instead.",
                json!({"type": "object", "properties": {
                    "text": s("One self-contained statement, e.g. use uv, not pip."),
                    "kind": {"type": "string", "enum": ["preference", "decision", "fact", "procedure", "result", "note"],
                             "description": "preference: a preference or standing instruction; decision: a choice made, with its \
                              reason; fact: about their work or life; procedure: how they do something; result: an outcome worth \
                              keeping; note: anything else. Results and notes are retrieved when relevant, not kept in the profile."},
                    "reason": s("Why it was decided. Required when kind is decision."),
                    "scope": {"type": "string", "enum": ["global", "project"], "default": "global",
                              "description": "global: everywhere. project: only in this session's workspace."}},
                    "required": ["text", "kind"]}),
            ),
            t(
                "recall",
                "Search stored memories: past conversations, episodes, imported history and anything not in the profile, \
                 including ones not yet matured. Use it when the user asks about the past. Each hit shows its id, kind, \
                 trust, date and provenance.",
                json!({"type": "object", "properties": {"query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 5}}, "required": ["query"]}),
            ),
            t(
                "forget",
                "Forget a memory by id. The record of it stays in the audit journal; its text does not.",
                json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            ),
            t(
                "correct",
                "Replace a memory's text with a corrected version, keeping its kind and scope. Use it, not remember, when \
                 something recorded has changed (recall finds the id). A decision keeps a reason only if the new text ends \
                 with (reason: ...). Refused if it would lower the memory's trust.",
                json!({"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}},
                    "required": ["id", "text"]}),
            ),
            t(
                "dispatch",
                "Send work to a crew agent that runs in the background in its own sandbox, and get its run id back \
                 immediately. Use it for long, parallel or bulky work: a literature sweep, an implementation in a git \
                 worktree, a long investigation. The result arrives later in this conversation. The crew cannot see this \
                 conversation, so the goal must carry the context it needs.",
                // Abridged: the role menu lists the profile's roles (three here).
                json!({"type": "object", "properties": {
                    "role": {"type": "string", "enum": ["researcher", "engineer", "reviewer"],
                             "description": "Who does the work. researcher: literature and evidence · engineer: builds in a \
                              worktree · reviewer: adversarial review"},
                    "goal": s("What to find out or build, with the facts, paths, constraints and definition of done the crew needs."),
                    "playbook": {"type": "string", "enum": ["deep_research", "build"], "description": "deep_research: \
                     plan, parallel scouts, adversarial verify, cited report. build: plan, implement in a worktree, test \
                     loop, review, diff for merge. Omit for a single role-prompt run (investigate, review, experiment)."},
                    "workspace": s("Directory the run works on; default is this session's workspace."),
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
        assert!(matches!(host.consequence(&plan), Consequence::Ask { .. }));
        let ask = marlowe_agent::ToolCallReq { id: "q".into(), name: "ask_user".into(), args: json!({"questions": [{"question": "Which?", "options": [{"label": "a"}]}]}) };
        let out = host.call_scoped(&ask, &cx);
        assert!(out.is_error && out.content.contains("No one can answer"), "headless: refused with guidance");
    }
}
