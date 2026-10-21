//! What each built-in tool needs beyond its schema for the forgiving layer ([`marlowe_agent::args`]):
//! the argument names models send for it, op-dependent requirements, an example and a line for
//! known confusions. `ToolDef::new` looks its tool up here; `ToolDef::rules` overrides.
//!
//! A rule names keys by the tool's schema. One that names a key the shown schema does not have
//! does nothing (the layer skips it), so a rule may sit here ahead of the schema that grows it.

use marlowe_agent::args::{ArgRules, Fold, Require};

/// The rules for tool `name`: none beyond the generic ones for a tool not listed.
pub fn for_tool(name: &str) -> ArgRules {
    match name {
        "read" => ArgRules {
            aliases: &[("file_path", "path"), ("file", "path"), ("filename", "path"), ("notebook_path", "path"), ("start", "offset"), ("line", "offset"), ("lines", "limit"), ("count", "limit"), ("n", "limit")],
            example: Some(r#"read{"path": "src/main.rs", "offset": 120}"#),
            ..ArgRules::NONE
        },
        "write" => ArgRules {
            aliases: &[("file_path", "path"), ("file", "path"), ("filename", "path"), ("text", "content"), ("contents", "content"), ("data", "content"), ("body", "content")],
            example: Some(r#"write{"path": "notes/todo.md", "content": "- train v2"}"#),
            ..ArgRules::NONE
        },
        "edit" => ArgRules {
            aliases: &[("file_path", "path"), ("file", "path"), ("old", "old_string"), ("find", "old_string"), ("search", "old_string"), ("old_str", "old_string"), ("new", "new_string"), ("replace", "new_string"), ("replacement", "new_string"), ("new_str", "new_string")],
            example: Some(r#"edit{"path": "README.md", "old_string": "v0.3", "new_string": "v0.4"}"#),
            ..ArgRules::NONE
        },
        "multi_edit" => ArgRules {
            aliases: &[("file_path", "path"), ("changes", "edits"), ("replacements", "edits")],
            ..ArgRules::NONE
        },
        "notebook_edit" => ArgRules {
            aliases: &[("notebook_path", "path"), ("file_path", "path"), ("cell_id", "cell"), ("index", "cell"), ("source", "new_source"), ("content", "new_source"), ("mode", "op"), ("edit_mode", "op"), ("action", "op")],
            ..ArgRules::NONE
        },
        "edit_lines" => ArgRules {
            aliases: &[("file_path", "path"), ("content", "text"), ("new_text", "text"), ("lines", "text")],
            ..ArgRules::NONE
        },
        "glob" => ArgRules {
            aliases: &[("glob", "pattern"), ("query", "pattern"), ("name", "pattern"), ("dir", "path"), ("directory", "path"), ("cwd", "path"), ("head_limit", "limit"), ("max", "limit")],
            example: Some(r#"glob{"pattern": "**/*.rs"}"#),
            ..ArgRules::NONE
        },
        "grep" => ArgRules {
            aliases: &[("query", "pattern"), ("regex", "pattern"), ("search", "pattern"), ("dir", "path"), ("cwd", "path"), ("head_limit", "limit"), ("max", "limit"), ("context", "-C"), ("ignore_case", "-i"), ("case_insensitive", "-i"), ("after", "-A"), ("before", "-B")],
            example: Some(r#"grep{"pattern": "fn main", "path": "src"}"#),
            ..ArgRules::NONE
        },
        "bash" => ArgRules {
            aliases: &[("cmd", "command"), ("script", "command"), ("code", "command"), ("run_in_background", "background"), ("description", "label"), ("timeout", "timeout_ms")],
            example: Some(r#"bash{"command": "cargo test -p marlowe-agent"}"#),
            ..ArgRules::NONE
        },
        "powershell" => ArgRules {
            aliases: &[("cmd", "command"), ("script", "command"), ("code", "command"), ("run_in_background", "background"), ("description", "label"), ("timeout", "timeout_ms")],
            example: Some(r#"powershell{"command": "cargo test -p marlowe-agent"}"#),
            ..ArgRules::NONE
        },
        "python" => ArgRules {
            aliases: &[("command", "code"), ("script", "code"), ("source", "code"), ("file", "path"), ("script_path", "path"), ("argv", "args"), ("run_in_background", "background"), ("description", "label")],
            split: &["args"], requires: &[Require::OneOf(&["code", "path"])],
            ..ArgRules::NONE
        },
        "web_fetch" => ArgRules {
            aliases: &[("link", "url"), ("href", "url"), ("uri", "url"), ("page", "url"), ("source", "url"), ("url_or_arxiv_id", "url"), ("id", "url"), ("arxiv_id", "url"), ("question", "prompt"), ("ask", "prompt")],
            sniff_arxiv: true,
            example: Some(r#"web_fetch{"url": "https://arxiv.org/abs/2407.01234"}"#),
            see_also: Some("To search instead: web_search(query) or arxiv(query)."),
            ..ArgRules::NONE
        },
        "arxiv" => ArgRules {
            aliases: &[("q", "query"), ("search", "query"), ("keywords", "query"), ("terms", "query"), ("max", "limit"), ("max_results", "limit"), ("n", "limit")],
            example: Some(r#"arxiv{"query": "speculative decoding"}"#),
            see_also: Some("To read a paper: web_fetch(url)."),
            ..ArgRules::NONE
        },
        "web_search" => ArgRules {
            aliases: &[("q", "query"), ("search", "query"), ("keywords", "query"), ("domains", "allowed_domains"), ("site", "allowed_domains")],
            example: Some(r#"web_search{"query": "min-p sampling"}"#),
            see_also: Some("To read a known page: web_fetch(url)."),
            ..ArgRules::NONE
        },
        "symbols" => ArgRules {
            aliases: &[("name", "query"), ("symbol", "query"), ("q", "query"), ("max", "limit"), ("head_limit", "limit")],
            ..ArgRules::NONE
        },
        "definition" => ArgRules {
            aliases: &[("name", "symbol"), ("query", "symbol"), ("max", "limit")],
            ..ArgRules::NONE
        },
        "references" => ArgRules {
            aliases: &[("name", "symbol"), ("query", "symbol"), ("max", "limit")],
            ..ArgRules::NONE
        },
        "hover" => ArgRules {
            aliases: &[("name", "symbol"), ("query", "symbol")],
            ..ArgRules::NONE
        },
        "diagnostics" => ArgRules {
            aliases: &[("file", "path"), ("file_path", "path"), ("max", "limit")],
            ..ArgRules::NONE
        },
        "rename_symbol" => ArgRules {
            aliases: &[("name", "symbol"), ("old_name", "symbol"), ("from", "symbol"), ("to", "new_name"), ("new", "new_name")],
            ..ArgRules::NONE
        },
        "project" => ArgRules {
            aliases: &[("command", "op"), ("action", "op"), ("step", "op"), ("kind", "op"), ("extra", "args")],
            split: &["args"], folds: &[Fold { when: ("op", "run"), from: "command", to: "op" }],
            ..ArgRules::NONE
        },
        "service" => ArgRules {
            aliases: &[("action", "op"), ("cmd", "command"), ("tail", "limit"), ("lines", "limit")],
            ..ArgRules::NONE
        },
        "http_check" => ArgRules {
            aliases: &[("link", "url"), ("endpoint", "url"), ("query", "url"), ("status", "expect_status"), ("contains", "expect_contains")],
            ..ArgRules::NONE
        },
        "browser" => ArgRules {
            aliases: &[("link", "url"), ("page", "url"), ("path", "url"), ("action", "op"), ("mode", "op")],
            ..ArgRules::NONE
        },
        "todo" => ArgRules {
            aliases: &[("todos", "items"), ("tasks", "items"), ("list", "items"), ("subject", "content"), ("task", "content"), ("title", "content"), ("description", "content"), ("activeForm", "active_form")],
            ..ArgRules::NONE
        },
        "ask_user" => ArgRules {
            aliases: &[("question", "questions"), ("multiSelect", "multi_select")],
            ..ArgRules::NONE
        },
        "skill" => ArgRules {
            aliases: &[("skill", "name"), ("id", "name"), ("arguments", "args"), ("input", "args")],
            ..ArgRules::NONE
        },
        "agent" => ArgRules {
            aliases: &[("subagent_type", "type"), ("agent_type", "type"), ("kind", "type"), ("task", "prompt"), ("goal", "prompt"), ("instructions", "prompt"), ("description", "label"), ("title", "label"), ("run_in_background", "background")],
            example: Some(r#"agent{"type": "explore", "prompt": "Find where the config is parsed"}"#),
            see_also: Some("A workflow: workflow(script). Crew work: dispatch(role, prompt)."),
            ..ArgRules::NONE
        },
        "send_message" => ArgRules {
            aliases: &[("agent_id", "to"), ("id", "to"), ("recipient", "to"), ("run_id", "to"), ("message", "text"), ("body", "text"), ("content", "text")],
            ..ArgRules::NONE
        },
        "enter_plan" => ArgRules {
            aliases: &[("why", "reason"), ("description", "reason")],
            ..ArgRules::NONE
        },
        "exit_plan" => ArgRules {
            aliases: &[("text", "plan"), ("content", "plan"), ("markdown", "plan")],
            ..ArgRules::NONE
        },
        "enter_worktree" => ArgRules {
            aliases: &[("branch", "name")],
            ..ArgRules::NONE
        },
        "exit_worktree" => ArgRules {
            aliases: &[("discard", "discard_changes"), ("force", "discard_changes")],
            ..ArgRules::NONE
        },
        "git" => ArgRules {
            policy_keys: &["force", "force_with_lease", "force_if_includes", "mirror", "prune", "forced", "overwrite_remote", "no_verify", "skip_hooks", "no_hooks", "skip_verify", "noverify"],
            aliases: &[("command", "op"), ("action", "op"), ("path", "paths"), ("file", "paths"), ("files", "paths"), ("msg", "message"), ("m", "message"), ("base", "rev"), ("from", "rev"), ("commit", "rev"), ("sha", "rev"), ("ref", "rev"), ("revision", "rev"), ("name", "branch"), ("new_branch", "branch")],
            folds: &[Fold { when: ("op", "stash"), from: "action", to: "stash" }], requires: &[Require::When { key: "op", is: "commit", need: &["message"], unless: &["amend"] }],
            ..ArgRules::NONE
        },
        "gh" => ArgRules {
            policy_keys: &["force", "force_with_lease", "force_if_includes", "mirror", "prune", "forced", "overwrite_remote", "no_verify", "skip_hooks", "no_hooks", "skip_verify", "noverify"],
            aliases: &[("action", "op"), ("command", "op"), ("pr", "number"), ("issue", "number"), ("id", "number"), ("head", "branch"), ("head_sha", "sha"), ("commit", "sha"), ("text", "body"), ("message", "body")],
            values: &[("op", "pr_checks", "ci")], requires: &[Require::When { key: "op", is: "pr_create", need: &["title"], unless: &[] }, Require::When { key: "op", is: "pr_reply", need: &["number", "comment_id", "body"], unless: &[] }, Require::When { key: "op", is: "issue_view", need: &["number"], unless: &[] }],
            ..ArgRules::NONE
        },
        "workflow" => ArgRules {
            aliases: &[
                ("code", "script"), ("source", "script"), ("starlark", "script"), ("script_path", "path"), ("file", "path"), ("workflow", "name"), ("input", "args"), ("params", "args"),
                ("each", "items"), ("for_each", "items"), ("over", "items_from"), ("task", "prompt"), ("instructions", "prompt"),
                ("combine", "then"), ("synthesis", "then"), ("finally", "then"), ("readonly", "read_only"),
            ],
            ignore: &["role", "label", "phase", "phase_name", "description", "title", "item_type", "item"],
            example: Some(r#"workflow{"prompt": "Summarise {item}", "items_from": "folders", "then": "one overview"}"#),
            see_also: Some("One task: agent(type, prompt). Crew work: dispatch(role, prompt)."),
            ..ArgRules::NONE
        },
        "report_findings" => ArgRules {
            aliases: &[("issues", "findings"), ("results", "findings"), ("summary", "claim"), ("title", "claim"), ("path", "file")],
            ..ArgRules::NONE
        },
        "task_output" => ArgRules {
            aliases: &[("task_id", "id"), ("job_id", "id"), ("job", "id"), ("since", "offset"), ("until_regex", "until")],
            ..ArgRules::NONE
        },
        "task_stop" => ArgRules {
            aliases: &[("task_id", "id"), ("job_id", "id"), ("run_id", "id"), ("wf_id", "id")],
            example: Some(r#"task_stop{"id": "wf_3a1"}"#),
            ..ArgRules::NONE
        },
        "monitor" => ArgRules {
            aliases: &[("cmd", "command"), ("description", "label")],
            requires: &[Require::UnlessAny { need: "command", unless: &["id"] }],
            see_also: Some("A file or folder: watch(op, path)."),
            ..ArgRules::NONE
        },
        "schedule" => ArgRules {
            aliases: &[("once_at", "at"), ("time", "at"), ("when", "at"), ("delay", "delay_s"), ("seconds", "delay_s"), ("in", "delay_s")],
            requires: &[Require::OneOf(&["delay_s", "at", "cron"])],
            ..ArgRules::NONE
        },
        "watch" => ArgRules {
            aliases: &[("action", "op"), ("dir", "path"), ("folder", "path")],
            see_also: Some("A command on an interval: monitor(command)."),
            ..ArgRules::NONE
        },
        "notify" => ArgRules {
            aliases: &[("subject", "title"), ("heading", "title"), ("text", "body"), ("message", "body")],
            ..ArgRules::NONE
        },
        "send_file" => ArgRules {
            aliases: &[("file", "path"), ("file_path", "path"), ("message", "note"), ("caption", "note"), ("text", "note")],
            ..ArgRules::NONE
        },
        "notices" => ArgRules {
            aliases: &[("max", "limit"), ("n", "limit")],
            ..ArgRules::NONE
        },
        _ => ArgRules::NONE,
    }
}
