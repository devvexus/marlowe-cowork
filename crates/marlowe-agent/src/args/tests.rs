//! The forgiving layer: names, keys, types, requirements and the text a mistake gets.

use std::sync::Arc;

use marlowe_model::{CancelToken, ToolCall, ToolSpec};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use super::*;
use crate::middleware::ToolMiddleware;
use crate::tools::{CallCtx, Consequence, SessionScope, ToolCallReq, ToolCtx, ToolManifest};

fn spec(name: &str, props: Value, required: &[&str]) -> ToolSpec {
    ToolSpec { name: name.into(), description: format!("{name} tool"), input_schema: json!({"type": "object", "properties": props, "required": required}) }
}

fn web_fetch() -> ToolSpec {
    spec(
        "web_fetch",
        json!({"url": {}, "prompt": {}, "offset": {"type": "integer"}, "part": {"type": "string", "enum": ["text", "references"]}, "raw": {"type": "boolean"}}),
        &["url"],
    )
}

fn web_fetch_rules() -> ArgRules {
    ArgRules {
        example: Some(r#"web_fetch{"url": "https://arxiv.org/abs/2407.01234"}"#),
        see_also: Some("To search instead: web_search(query) or arxiv(query)."),
        sniff_arxiv: true,
        ..ArgRules::NONE
    }
}

fn read() -> (ToolSpec, ArgRules) {
    (
        spec("read", json!({"path": {}, "offset": {"type": "integer"}, "limit": {"type": "integer"}, "pages": {}}), &["path"]),
        ArgRules { aliases: &[("start", "offset"), ("line", "offset"), ("lines", "limit")], ..ArgRules::NONE },
    )
}

fn ok(spec: &ToolSpec, rules: &ArgRules, args: Value) -> Normalized {
    normalize(spec, rules, &args).unwrap_or_else(|e| panic!("{args} should be fine: {e:?}"))
}

fn bad(spec: &ToolSpec, rules: &ArgRules, args: Value) -> String {
    let err = normalize(spec, rules, &args).expect_err(&format!("{args} should be refused"));
    invalid_text(spec, rules, &err, false)
}

// ---------------------------------------------------------------- names

struct Names(Vec<ToolSpec>);

impl ToolHost for Names {
    fn specs(&self) -> Vec<ToolSpec> {
        self.0.clone()
    }
    fn call(&self, _: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok("ran", "ok")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn names_host(names: &[&str]) -> Names {
    Names(names.iter().map(|n| spec(n, json!({"url": {}, "id": {}, "watch": {"type": "boolean"}, "op": {}}), &[])).collect())
}

fn resolved(host: &dyn ToolHost, name: &str, args: Value) -> (String, Value) {
    let mut c = ToolCall { id: "c".into(), name: name.into(), args };
    prepare_call(host, &mut c);
    (c.name, c.args)
}

#[test]
fn a_name_resolves_exactly_then_by_case_and_separators() {
    let h = names_host(&["web_fetch", "bash", "task_output", "web_search"]);
    for sent in ["web_fetch", "WebFetch", "web-fetch", "Web_Fetch", "webfetch", " web_fetch "] {
        assert_eq!(resolved(&h, sent, json!({})).0, "web_fetch", "{sent}");
    }
    assert_eq!(resolved(&h, "Bash", json!({})).0, "bash");
    assert_eq!(resolved(&h, "nothing_like_it", json!({})).0, "nothing_like_it", "an unknown name is left for the layer to answer");
}

#[test]
fn legacy_names_resolve_to_the_merged_tool_and_inject_what_they_implied() {
    let h = names_host(&["web_fetch", "task_output", "task_stop", "task_list", "schedule", "todo", "gh"]);
    assert_eq!(resolved(&h, "paper", json!({"url": "u"})), ("web_fetch".into(), json!({"url": "u"})));
    assert_eq!(resolved(&h, "job_output", json!({"id": "j"})).0, "task_output");
    assert_eq!(resolved(&h, "job_watch", json!({"id": "j"})), ("task_output".into(), json!({"id": "j", "watch": true})));
    assert_eq!(resolved(&h, "job_kill", json!({})).0, "task_stop");
    assert_eq!(resolved(&h, "cron_delete", json!({})).0, "task_stop");
    assert_eq!(resolved(&h, "cron_create", json!({})).0, "schedule");
    assert_eq!(resolved(&h, "schedule_wakeup", json!({})).0, "schedule");
    assert_eq!(resolved(&h, "cron_list", json!({})).0, "task_list");
    assert_eq!(resolved(&h, "list_agents", json!({})).0, "task_list");
    for old in ["todo_add", "todo_update", "todo_get"] {
        assert_eq!(resolved(&h, old, json!({})).0, "todo", "{old}");
    }
    assert_eq!(resolved(&h, "ci_status", json!({})), ("gh".into(), json!({"op": "ci"})));
    assert_eq!(resolved(&h, "Job-Watch", json!({})).0, "task_output", "legacy names are spelling-insensitive too");
    // `workflow` was renamed `team`: old transcripts and saved calls still reach it.
    let t = names_host(&["team"]);
    assert_eq!(resolved(&t, "workflow", json!({"name": "code-review"})), ("team".into(), json!({"name": "code-review"})));
    assert_eq!(resolved(&t, "team_task", json!({})).0, "team");
}

#[test]
fn a_legacy_name_is_left_alone_while_the_host_still_has_it_or_lacks_the_target() {
    // The host still runs `paper`: nothing moves.
    let h = names_host(&["web_fetch", "paper"]);
    assert_eq!(resolved(&h, "paper", json!({})).0, "paper");
    // The target is not there yet: the table is wired ahead of the merge and does nothing.
    let h = names_host(&["web_fetch"]);
    assert_eq!(resolved(&h, "todo_add", json!({})).0, "todo_add");
    assert_eq!(resolved(&h, "ci_status", json!({})).0, "ci_status");
}

#[test]
fn an_mcp_tool_resolves_without_its_prefix() {
    let h = names_host(&["bash", "mcp__web__echo"]);
    assert_eq!(resolved(&h, "web__echo", json!({})).0, "mcp__web__echo");
    assert_eq!(resolved(&h, "web.echo", json!({})).0, "mcp__web__echo");
    assert_eq!(resolved(&h, "web.nothing", json!({})).0, "web.nothing");
}

#[test]
fn the_legacy_table_is_data_with_no_duplicate_names() {
    let mut seen = std::collections::HashSet::new();
    for l in LEGACY {
        assert!(seen.insert(l.from), "{} is listed twice", l.from);
        assert_ne!(l.from, l.to);
        if let Some((_, v)) = l.inject {
            assert!(serde_json::from_str::<Value>(v).is_ok(), "{} injects invalid JSON {v}", l.from);
        }
    }
    assert!(LEGACY.len() >= 13);
}

#[test]
fn an_allowlist_or_denylist_naming_an_old_tool_means_the_tool_that_runs_it() {
    assert_eq!(canonical_tool("cron_create"), "schedule");
    assert_eq!(canonical_tool("job_kill"), "task_stop");
    assert_eq!(canonical_tool("paper"), "web_fetch");
    assert_eq!(canonical_tool("workflow"), "team", "a crew allowlist or denylist naming `workflow` means `team`");
    assert_eq!(canonical_tool("read"), "read");
}

// ---------------------------------------------------------------- container

#[test]
fn the_arguments_container_is_made_an_object() {
    let (read, _) = read();
    let c = |v: Value| container(&read, &v);
    assert_eq!(c(Value::Null).unwrap(), Map::new());
    assert_eq!(c(json!("{\"path\": \"a.rs\"}")).unwrap()["path"], "a.rs", "a double-encoded object");
    assert_eq!(c(json!("a.rs")).unwrap()["path"], "a.rs", "a bare string is the one required string argument");
    assert!(c(json!([1, 2])).is_err());
    assert!(c(json!(7)).is_err());
    // Two required arguments: a bare string is no answer.
    let edit = spec("edit", json!({"path": {}, "old_string": {}}), &["path", "old_string"]);
    assert!(container(&edit, &json!("a.rs")).is_err());
    // An enum is not text to take whole.
    let op = spec("watch", json!({"op": {"enum": ["start", "stop"]}}), &["op"]);
    assert!(container(&op, &json!("start")).is_err());
}

#[test]
fn prepare_call_makes_the_arguments_an_object_for_the_tool_it_names() {
    let h = Names(vec![read().0]);
    assert_eq!(resolved(&h, "read", json!("src/main.rs")).1, json!({"path": "src/main.rs"}));
    assert_eq!(resolved(&h, "read", Value::Null).1, json!({}));
}

// ---------------------------------------------------------------- keys

#[test]
fn keys_are_read_by_spelling_alias_and_synonym() {
    let (r, rules) = read();
    for (sent, key) in [("filePath", "path"), ("FILE_PATH", "path"), ("file_path", "path"), ("file", "path"), ("filename", "path")] {
        let n = ok(&r, &rules, json!({ sent: "a.rs" }));
        assert_eq!(n.args, json!({ key: "a.rs" }), "{sent}");
        assert_eq!(n.rewrites.len(), 1);
    }
    // The tool's own aliases, and a type fix on the way.
    assert_eq!(ok(&r, &rules, json!({"path": "a", "start": "40", "lines": 20})).args, json!({"path": "a", "offset": 40, "limit": 20}));
    // Synonyms need the target key on the tool: `head_limit` -> `limit` here, not for a tool without one.
    assert_eq!(ok(&r, &rules, json!({"path": "a", "head_limit": 5})).args["limit"], 5);
    let no_limit = spec("stat", json!({"path": {}}), &["path"]);
    assert!(normalize(&no_limit, &ArgRules::NONE, &json!({"path": "a", "head_limit": 5})).is_err());
}

#[test]
fn grep_flags_match_literally_first_then_by_spelling() {
    let grep = spec("grep", json!({"pattern": {}, "-A": {"type": "integer"}, "-B": {"type": "integer"}, "-i": {"type": "boolean"}, "limit": {"type": "integer"}}), &["pattern"]);
    let rules = ArgRules { aliases: &[("after", "-A"), ("ignore_case", "-i")], ..ArgRules::NONE };
    assert_eq!(ok(&grep, &rules, json!({"pattern": "x", "-A": 2, "-i": "true"})).args, json!({"pattern": "x", "-A": 2, "-i": true}));
    assert_eq!(ok(&grep, &rules, json!({"pattern": "x", "after": 2, "ignore_case": true})).args, json!({"pattern": "x", "-A": 2, "-i": true}));
    assert_eq!(ok(&grep, &rules, json!({"pattern": "x", "--limit": 4})).args["limit"], 4, "leading dashes are ignored for a spelling match");
}

#[test]
fn the_same_value_under_two_names_is_one_and_two_values_are_an_error() {
    let (r, rules) = read();
    assert_eq!(ok(&r, &rules, json!({"path": "a", "file_path": "a"})).args, json!({"path": "a"}));
    let text = bad(&r, &rules, json!({"path": "a", "file_path": "b"}));
    assert!(text.contains("unknown `file_path`"), "{text}");
}

#[test]
fn a_url_under_the_wrong_key_is_the_url_but_a_non_url_never_is() {
    let rules = web_fetch_rules();
    let wf = web_fetch();
    let n = ok(&wf, &rules, json!({"query": "https://arxiv.org/abs/2407.01234"}));
    assert_eq!(n.args, json!({"url": "https://arxiv.org/abs/2407.01234"}));
    assert_eq!(n.rewrites[0].from, "query");
    assert_eq!(ok(&wf, &rules, json!({"source": "http://example.com/a"})).args["url"], "http://example.com/a");
    // An arXiv id, for the tool that reads papers.
    assert_eq!(ok(&wf, &rules, json!({"query": "2407.01234v2"})).args["url"], "2407.01234v2");
    assert_eq!(ok(&wf, &rules, json!({"id": "hep-th/9901001"})).args["url"], "hep-th/9901001");
    // Not a URL: never moved into `url`.
    let text = bad(&wf, &rules, json!({"query": "min-p sampling"}));
    assert_eq!(
        text,
        "web_fetch was not run: missing `url`; unknown `query` (\"min-p sampling\" is not a URL).\n\
         web_fetch(url, offset?: int, part?: text|references, prompt?, raw?: bool)\n\
         e.g. web_fetch{\"url\": \"https://arxiv.org/abs/2407.01234\"}\n\
         To search instead: web_search(query) or arxiv(query)."
    );
    // An arXiv id means nothing to a tool that does not read papers.
    let plain = ArgRules::NONE;
    assert!(normalize(&wf, &plain, &json!({"query": "2407.01234"})).is_err());
}

#[test]
fn one_stray_key_and_one_missing_required_key_are_paired_when_the_value_fits() {
    let t = spec("notify", json!({"title": {}, "body": {}, "level": {"type": "integer"}}), &["title"]);
    assert_eq!(ok(&t, &ArgRules::NONE, json!({"heading": "Done"})).args, json!({"title": "Done"}));
    // The value must fit: text is no integer.
    let n = spec("wait", json!({"seconds": {"type": "integer"}}), &["seconds"]);
    assert!(normalize(&n, &ArgRules::NONE, &json!({"delay": "soon"})).is_err());
    assert_eq!(ok(&n, &ArgRules::NONE, json!({"delay": "30"})).args, json!({"seconds": 30}));
    // Two stray keys, or two missing: no guess between candidates.
    let two = spec("two", json!({"a": {}, "b": {}}), &["a", "b"]);
    let text = bad(&two, &ArgRules::NONE, json!({"x": "1"}));
    assert!(text.contains("missing `a`") && text.contains("missing `b`") && text.contains("unknown `x`"), "{text}");
    let text = bad(&t, &ArgRules::NONE, json!({"x": "1", "y": "2"}));
    assert!(text.contains("unknown `x`") && text.contains("unknown `y`"), "{text}");
}

#[test]
fn a_url_is_never_moved_into_query_and_a_non_url_never_into_url_by_the_rescue() {
    let search = spec("web_search", json!({"query": {}}), &["query"]);
    let text = bad(&search, &ArgRules::NONE, json!({"target": "https://example.com/x"}));
    assert!(text.contains("missing `query`") && text.contains("unknown `target`"), "{text}");
    let fetch = web_fetch();
    assert!(normalize(&fetch, &ArgRules::NONE, &json!({"topic": "rust lifetimes"})).is_err());
}

#[test]
fn op_folds_and_value_renames_apply_only_when_the_schema_has_the_target() {
    let project = spec("project", json!({"op": {"type": "string", "enum": ["test", "build", "gate"]}, "args": {"type": "array", "items": {"type": "string"}}}), &[]);
    let rules = ArgRules { folds: &[Fold { when: ("op", "run"), from: "command", to: "op" }], split: &["args"], ..ArgRules::NONE };
    assert_eq!(ok(&project, &rules, json!({"op": "run", "command": "test"})).args, json!({"op": "test"}));
    assert_eq!(ok(&project, &rules, json!({"op": "test", "args": "-p marlowe-agent --lib \"a b\""})).args["args"], json!(["-p", "marlowe-agent", "--lib", "a b"]));
    // `ci` is not in the enum yet: the rename waits.
    let gh = spec("gh", json!({"op": {"type": "string", "enum": ["pr_checks", "issue_view"]}}), &["op"]);
    let rules = ArgRules { values: &[("op", "pr_checks", "ci")], ..ArgRules::NONE };
    assert_eq!(ok(&gh, &rules, json!({"op": "pr_checks"})).args["op"], "pr_checks");
    let gh = spec("gh", json!({"op": {"type": "string", "enum": ["ci", "issue_view"]}}), &["op"]);
    assert_eq!(ok(&gh, &rules, json!({"op": "pr_checks"})).args["op"], "ci");
    // A fold whose source key is a real property of the schema does nothing: `command` is the
    // command of `op: run` there.
    let project_with_command = spec("project", json!({"op": {}, "command": {}}), &[]);
    assert_eq!(ok(&project_with_command, &rules_run_fold(), json!({"op": "run", "command": "test"})).args, json!({"op": "run", "command": "test"}));
    // A fold whose target key the schema lacks does nothing (and the stray key is an error).
    let git = spec("git", json!({"op": {}}), &["op"]);
    let rules = ArgRules { folds: &[Fold { when: ("op", "stash"), from: "action", to: "stash" }], ..ArgRules::NONE };
    assert!(normalize(&git, &rules, &json!({"op": "stash", "action": "pop"})).is_err());
}

fn rules_run_fold() -> ArgRules {
    ArgRules { folds: &[Fold { when: ("op", "run"), from: "command", to: "op" }], ..ArgRules::NONE }
}

// ---------------------------------------------------------------- types

#[test]
fn types_are_coerced_losslessly() {
    let s = spec(
        "t",
        json!({
            "n": {"type": "integer"}, "x": {"type": "number"}, "flag": {"type": "boolean"},
            "paths": {"type": "array", "items": {"type": "string"}},
            "edits": {"type": "array", "items": {"type": "object", "properties": {"find": {}, "count": {"type": "integer"}}}},
            "obj": {"type": "object", "properties": {"k": {"type": "integer"}}},
            "num_as_text": {}, "mode": {"type": "string", "enum": ["text", "references", "raw"]}
        }),
        &[],
    );
    let r = ArgRules::NONE;
    assert_eq!(ok(&s, &r, json!({"n": "12"})).args["n"], 12);
    assert_eq!(ok(&s, &r, json!({"n": 12.0})).args["n"], 12);
    assert_eq!(ok(&s, &r, json!({"n": "12.0"})).args["n"], 12);
    assert_eq!(ok(&s, &r, json!({"x": "1.5"})).args["x"], 1.5);
    assert_eq!(ok(&s, &r, json!({"flag": "true"})).args["flag"], true);
    assert_eq!(ok(&s, &r, json!({"flag": "False"})).args["flag"], false);
    assert_eq!(ok(&s, &r, json!({"paths": "a.rs"})).args["paths"], json!(["a.rs"]), "a scalar becomes a list");
    assert_eq!(ok(&s, &r, json!({"paths": "[\"a\", \"b\"]"})).args["paths"], json!(["a", "b"]), "a JSON-encoded list is parsed");
    assert_eq!(ok(&s, &r, json!({"edits": {"find": "a", "count": "2"}})).args["edits"], json!([{"find": "a", "count": 2}]), "an object where a list of objects is expected");
    assert_eq!(ok(&s, &r, json!({"obj": "{\"k\": \"3\"}"})).args["obj"], json!({"k": 3}), "a JSON-encoded object is parsed");
    assert_eq!(ok(&s, &r, json!({"num_as_text": 42})).args["num_as_text"], "42", "a number for a text argument");
    assert_eq!(ok(&s, &r, json!({"mode": "TEXT"})).args["mode"], "text", "an enum, case-insensitively");
    assert_eq!(ok(&s, &r, json!({"mode": "ref"})).args["mode"], "references", "an enum, by unique prefix");
}

#[test]
fn what_is_not_lossless_is_an_error_with_the_reason() {
    let s = spec("t", json!({"n": {"type": "integer"}, "flag": {"type": "boolean"}, "mode": {"type": "string", "enum": ["raw", "references"]}, "x": {"type": "number"}}), &[]);
    let r = ArgRules::NONE;
    let text = bad(&s, &r, json!({"n": "abc"}));
    assert!(text.contains("`n` must be an integer (got \"abc\")"), "{text}");
    assert!(bad(&s, &r, json!({"n": 1.5})).contains("`n` must be an integer"));
    assert!(bad(&s, &r, json!({"flag": "maybe"})).contains("`flag` must be true or false"));
    assert!(bad(&s, &r, json!({"flag": 1})).contains("`flag`"));
    assert!(bad(&s, &r, json!({"x": [1]})).contains("`x`"));
    // "r" starts both `raw` and `references`: no guess.
    let text = bad(&s, &r, json!({"mode": "r"}));
    assert!(text.contains("ambiguous") && text.contains("raw|references"), "{text}");
    assert!(bad(&s, &r, json!({"mode": "nope"})).contains("`mode` must be one of raw|references"));
}

#[test]
fn ask_user_options_given_as_strings_become_labels() {
    let ask = spec(
        "ask_user",
        json!({"questions": {"type": "array", "items": {"type": "object", "properties": {
            "question": {}, "options": {"type": "array", "items": {"type": "object", "properties": {"label": {}, "description": {}}}}
        }}}}),
        &["questions"],
    );
    let n = ok(&ask, &ArgRules::NONE, json!({"questions": [{"question": "Which?", "options": ["a", "b"]}]}));
    assert_eq!(n.args["questions"][0]["options"], json!([{"label": "a"}, {"label": "b"}]));
}

#[test]
fn free_text_is_never_trimmed_unescaped_or_rewritten() {
    let s = spec("bash", json!({"command": {}, "timeout_ms": {"type": "integer"}, "label": {}}), &["command"]);
    let cmd = "  echo \"a\\nb\"  \n  && ls \\\n";
    let n = ok(&s, &ArgRules::NONE, json!({"command": cmd}));
    assert_eq!(n.args["command"], cmd);
    assert!(n.rewrites.is_empty());
    // A unit is never guessed: seconds stay what the model wrote.
    assert_eq!(ok(&s, &ArgRules::NONE, json!({"command": "x", "timeout_ms": 30})).args["timeout_ms"], 30);
    // A required value is never invented.
    assert!(bad(&s, &ArgRules::NONE, json!({"label": "x"})).contains("missing `command`"));
    // A text argument that holds a list or object is an error, not JSON-stringified.
    assert!(bad(&s, &ArgRules::NONE, json!({"command": ["ls"]})).contains("`command` must be text"));
}

#[test]
fn unknown_keys_are_never_dropped_silently() {
    let (r, rules) = read();
    let text = bad(&r, &rules, json!({"path": "a", "colour": "red"}));
    assert!(text.contains("unknown `colour`"), "{text}");
    // An open schema takes extra keys.
    let open = ToolSpec { name: "free".into(), description: "".into(), input_schema: json!({"type": "object", "properties": {"a": {}}, "additionalProperties": true}) };
    assert_eq!(ok(&open, &ArgRules::NONE, json!({"a": "1", "zzz": 2})).args["zzz"], 2);
}

#[test]
fn a_null_is_an_argument_not_given_for_a_built_in_and_left_alone_for_a_third_party() {
    let (r, rules) = read();
    assert_eq!(ok(&r, &rules, json!({"path": "a", "offset": null, "limit": null})).args, json!({"path": "a"}));
    assert!(bad(&r, &rules, json!({"path": null})).contains("missing `path`"));
    let t = spec("mcp__srv__q", json!({"n": {"type": ["integer", "null"]}}), &[]);
    assert_eq!(ok(&t, &ArgRules::NONE, json!({"n": null})).args, json!({"n": null}));
}

#[test]
fn a_union_type_takes_any_of_its_types() {
    let s = spec("t", json!({"v": {"type": ["string", "integer"]}, "w": {"anyOf": [{"type": "string"}, {"type": "array"}]}}), &[]);
    let r = ArgRules::NONE;
    assert_eq!(ok(&s, &r, json!({"v": 5, "w": ["a"]})).args, json!({"v": 5, "w": ["a"]}));
    assert_eq!(ok(&s, &r, json!({"v": "x", "w": "y"})).args, json!({"v": "x", "w": "y"}));
    assert!(bad(&s, &r, json!({"v": [1]})).contains("`v`"));
}

// ---------------------------------------------------------------- requirements

#[test]
fn requirements_the_schema_cannot_say() {
    let python = spec("python", json!({"code": {}, "path": {}}), &[]);
    let rules = ArgRules { requires: &[Require::OneOf(&["code", "path"])], ..ArgRules::NONE };
    assert!(bad(&python, &rules, json!({})).contains("missing one of `code`|`path`"));
    assert!(bad(&python, &rules, json!({"code": "1", "path": "a.py"})).contains("send only one of `code`|`path`"));
    ok(&python, &rules, json!({"code": "print(1)"}));
    ok(&python, &rules, json!({"path": "a.py"}));

    let git = spec("git", json!({"op": {}, "message": {}, "amend": {"type": "boolean"}}), &["op"]);
    let rules = ArgRules { requires: &[Require::When { key: "op", is: "commit", need: &["message"], unless: &["amend"] }], ..ArgRules::NONE };
    assert!(bad(&git, &rules, json!({"op": "commit"})).contains("missing `message` for op `commit`"));
    ok(&git, &rules, json!({"op": "commit", "message": "m"}));
    ok(&git, &rules, json!({"op": "commit", "amend": true}));
    ok(&git, &rules, json!({"op": "status"}));

    let service = spec("service", json!({"op": {}, "name": {}, "command": {}}), &["op"]);
    let rules = ArgRules { requires: &[Require::WhenAny { key: "op", is: "start", any: &["name", "command"] }], ..ArgRules::NONE };
    assert!(bad(&service, &rules, json!({"op": "start"})).contains("missing one of `name`|`command` for op `start`"));
    ok(&service, &rules, json!({"op": "start", "command": "serve"}));

    let monitor = spec("monitor", json!({"command": {}, "id": {}}), &[]);
    let rules = ArgRules { requires: &[Require::UnlessAny { need: "command", unless: &["id"] }], ..ArgRules::NONE };
    assert!(bad(&monitor, &rules, json!({})).contains("missing `command`"));
    ok(&monitor, &rules, json!({"id": "m1"}));
}

#[test]
fn a_requirement_naming_a_key_the_schema_lacks_waits_for_the_schema_that_has_it() {
    let schedule = spec("schedule", json!({"prompt": {}}), &["prompt"]);
    let rules = ArgRules { requires: &[Require::OneOf(&["delay_s", "at", "cron"])], ..ArgRules::NONE };
    ok(&schedule, &rules, json!({"prompt": "x"}));
}

// ---------------------------------------------------------------- mcp

#[test]
fn third_party_schemas_get_only_lossless_coercions_and_their_own_required() {
    let t = spec("mcp__srv__fetch", json!({"url": {"type": "string"}, "depth": {"type": "integer"}, "anything": {}}), &["url"]);
    let r = ArgRules::NONE;
    // No synonym, no sniffing, no rescue.
    let text = bad(&t, &r, json!({"link": "https://example.com"}));
    assert!(text.contains("missing `url`"), "{text}");
    assert!(!text.contains("unknown"), "an open schema takes the extra key: {text}");
    // Lossless coercions still apply.
    assert_eq!(ok(&t, &r, json!({"url": "u", "depth": "3"})).args["depth"], 3);
    // An untyped property is any value, not text.
    assert_eq!(ok(&t, &r, json!({"url": "u", "anything": 5})).args["anything"], 5);
    assert_eq!(ok(&t, &r, json!({"url": "u", "anything": {"k": 1}})).args["anything"], json!({"k": 1}));
    // A schema that closes itself is held to it.
    let closed = ToolSpec {
        name: "mcp__srv__x".into(),
        description: String::new(),
        input_schema: json!({"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": false}),
    };
    assert!(bad(&closed, &r, json!({"a": "1", "b": 2})).contains("unknown `b`"));
    // And a name that is not the server's gets the signature.
    assert_eq!(signature(&t), "mcp__srv__fetch(url, anything?, depth?: int)");
}

// ---------------------------------------------------------------- text

#[test]
fn signatures_show_types_enums_and_nested_shapes() {
    let s = spec(
        "ask_user",
        json!({
            "questions": {"type": "array", "items": {"type": "object", "properties": {"question": {}, "multi_select": {"type": "boolean"}}, "required": ["question"]}},
            "tags": {"type": "array", "items": {"type": "string"}},
            "nums": {"type": "array", "items": {"type": "integer"}},
            "part": {"enum": ["text", "references"]}
        }),
        &["questions"],
    );
    assert_eq!(signature(&s), "ask_user(questions: [{question, multi_select?: bool}], nums?: [int], part?: text|references, tags?: [string])");
    assert_eq!(signature(&spec("noargs", json!({}), &[])), "noargs()");
}

#[test]
fn the_error_is_at_most_four_lines_and_a_repeat_leads_with_the_example() {
    let wf = web_fetch();
    let rules = web_fetch_rules();
    let err = normalize(&wf, &rules, &json!({"query": "x y"})).unwrap_err();
    let first = invalid_text(&wf, &rules, &err, false);
    assert_eq!(first.lines().count(), 4);
    assert!(first.starts_with("web_fetch was not run:"));
    let again = invalid_text(&wf, &rules, &err, true);
    assert_eq!(again.lines().count(), 4);
    let lines: Vec<&str> = again.lines().collect();
    assert!(lines[0].starts_with("Same invalid call again: web_fetch was not run:"), "{again}");
    assert!(lines[1].starts_with("e.g. "), "the example comes first: {again}");
    assert!(lines[2].starts_with("web_fetch(url"));
    // No example, no see-also: two lines.
    let plain = invalid_text(&wf, &ArgRules::NONE, &err, false);
    assert_eq!(plain.lines().count(), 2);
}

#[test]
fn an_unknown_tool_gets_the_nearest_signature_and_the_names() {
    let specs = vec![web_fetch(), read().0, spec("web_search", json!({"query": {}}), &["query"])];
    let text = unknown_tool_text("fetch", &specs);
    assert!(text.starts_with("No tool `fetch`. Did you mean web_fetch(url, offset?: int"), "{text}");
    assert!(text.ends_with("Tools: web_fetch, read, web_search"), "{text}");
    let text = unknown_tool_text("raed", &specs);
    assert!(text.contains("Did you mean read(path"), "{text}");
    let text = unknown_tool_text("zzzzzz", &specs);
    assert_eq!(text, "No tool `zzzzzz`.\nTools: web_fetch, read, web_search");
}

// ---------------------------------------------------------------- the middleware

struct Shown {
    specs: Vec<(ToolSpec, ArgRules)>,
    hidden: Vec<String>,
}

impl ToolHost for Shown {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.iter().map(|(s, _)| s.clone()).collect()
    }
    fn call(&self, _: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok("ran", "ok")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
    fn arg_rules(&self, name: &str) -> Option<ArgRules> {
        self.specs.iter().find(|(s, _)| s.name == name).map(|(_, r)| *r)
    }
    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.hidden.iter().any(|h| h == name).then(|| ToolManifest {
            group: crate::tools::ToolGroup::Files,
            profiles: crate::tools::Profiles::ALL,
            effects: crate::tools::Effects::READ,
            path_args: &[],
        })
    }
}

fn layer() -> (ArgsLayer, CallCtx) {
    let (r, rr) = read();
    let host: Arc<dyn ToolHost> = Arc::new(Shown { specs: vec![(web_fetch(), web_fetch_rules()), (r, rr)], hidden: vec!["held_back".into()] });
    let cx = CallCtx::new(ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false), SessionScope::default());
    (ArgsLayer::new(host), cx)
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), args }
}

#[test]
fn the_layer_rewrites_the_call_before_anything_else_sees_it() {
    let (l, cx) = layer();
    let mut c = call("c1", "web_fetch", json!({"query": "https://example.com/x", "offset": "5"}));
    assert_eq!(l.before_approval(&mut c, &cx), CallFlow::Continue);
    assert_eq!(c.args, json!({"url": "https://example.com/x", "offset": 5}));
    let logged = cx.scope.with_ext(|e| e.get::<ArgRewrites>().map(|r| r.0.clone()).unwrap_or_default());
    assert_eq!(logged.len(), 2);
    assert!(logged.iter().any(|r| r.from == "query" && r.to == "url" && !r.coerced));
    assert!(logged.iter().any(|r| r.from == "offset" && r.coerced));
}

#[test]
fn a_renamed_key_is_told_once_per_tool_and_key() {
    let (l, cx) = layer();
    let run = |id: &str, args: Value| {
        let mut c = call(id, "read", args);
        assert_eq!(l.before_approval(&mut c, &cx), CallFlow::Continue);
        let mut out = ToolOutcome::ok("body", "1 line");
        l.after_call(&c, &cx, &mut out);
        out.content
    };
    let first = run("a", json!({"file_path": "x"}));
    assert!(first.ends_with("[args: read `file_path` as `path`; the parameter is `path`]"), "{first}");
    assert_eq!(run("b", json!({"file_path": "y"})), "body", "told once");
    assert!(run("c", json!({"filename": "y"})).contains("read `filename` as `path`"), "a different key is told");
    assert_eq!(run("d", json!({"path": "y"})), "body", "a correct call has no footer");
    // A coerced value alone says nothing.
    assert_eq!(run("e", json!({"path": "y", "offset": "3"})), "body");
}

#[test]
fn an_invalid_call_is_answered_without_asking_or_running_and_a_repeat_says_so() {
    let (l, cx) = layer();
    let mut c = call("c1", "web_fetch", json!({"query": "min-p sampling"}));
    let CallFlow::Done(out) = l.before_approval(&mut c, &cx) else { panic!("refused") };
    assert!(out.is_error);
    assert!(out.content.starts_with("web_fetch was not run: missing `url`; unknown `query`"), "{}", out.content);
    assert_eq!(out.summary, "invalid arguments");
    let mut c = call("c2", "web_fetch", json!({"query": "min-p sampling"}));
    let CallFlow::Done(again) = l.before_approval(&mut c, &cx) else { panic!("refused") };
    assert!(again.content.starts_with("Same invalid call again:"), "{}", again.content);
    // Something different is not "again".
    let mut c = call("c3", "web_fetch", json!({"query": "other words"}));
    let CallFlow::Done(other) = l.before_approval(&mut c, &cx) else { panic!("refused") };
    assert!(!other.content.starts_with("Same invalid call again:"));
}

#[test]
fn an_unknown_tool_is_answered_here_unless_the_host_runs_it_anyway() {
    let (l, cx) = layer();
    let mut c = call("c1", "fetch", json!({}));
    let CallFlow::Done(out) = l.before_approval(&mut c, &cx) else { panic!("refused") };
    assert!(out.is_error && out.content.starts_with("No tool `fetch`. Did you mean web_fetch(url"), "{}", out.content);
    assert!(out.content.contains("Tools: web_fetch, read"));
    // Held back by a wrapper but known to the host: the host answers for itself.
    let mut c = call("c2", "held_back", json!({}));
    assert_eq!(l.before_approval(&mut c, &cx), CallFlow::Continue);
}

#[test]
fn a_host_without_specs_is_left_alone() {
    let host: Arc<dyn ToolHost> = Arc::new(Shown { specs: vec![], hidden: vec![] });
    let l = ArgsLayer::new(host);
    let cx = CallCtx::new(ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false), SessionScope::default());
    let mut c = call("c1", "anything", json!({"x": 1}));
    assert_eq!(l.before_approval(&mut c, &cx), CallFlow::Continue);
    assert_eq!(c.args, json!({"x": 1}));
}

// ---------------------------------------------------------------- ignored keys

fn dispatch() -> (ToolSpec, ArgRules) {
    (
        spec("dispatch", json!({"role": {}, "prompt": {}, "playbook": {}, "cwd": {}}), &["role", "prompt"]),
        wired_rules("dispatch").unwrap(),
    )
}

#[test]
fn ignored_keys_are_dropped_with_a_rewrite_not_refused() {
    let (d, rules) = dispatch();
    let n = ok(&d, &rules, json!({"role": "researcher", "prompt": "go", "background": true, "label": "x"}));
    assert_eq!(n.args, json!({"role": "researcher", "prompt": "go"}));
    let dropped: Vec<&str> = n.rewrites.iter().filter(|r| r.to.is_empty()).map(|r| r.from.as_str()).collect();
    assert_eq!(dropped, ["background", "label"]);
    // A key the tool does list is never dropped, and an alias wins over ignoring.
    let w = spec("workflow", json!({"prompt": {}, "items": {"type": "array"}, "name": {}}), &[]);
    let r = ArgRules { aliases: &[("each", "items")], ignore: &["role", "each", "name"], ..ArgRules::NONE };
    let n = ok(&w, &r, json!({"prompt": "p", "each": ["a"], "role": "x", "name": "n"}));
    assert_eq!(n.args, json!({"prompt": "p", "items": ["a"], "name": "n"}));
    // Not listed: still an unknown key.
    assert!(normalize(&d, &rules, &json!({"role": "r", "prompt": "p", "colour": 1})).is_err());
}

#[test]
fn an_ignored_key_is_told_once_in_the_footer_and_the_call_runs() {
    let (d, rules) = dispatch();
    let host: Arc<dyn ToolHost> = Arc::new(Shown { specs: vec![(d, rules)], hidden: vec![] });
    let cx = CallCtx::new(ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false), SessionScope::default());
    let l = ArgsLayer::new(host);
    let run = |id: &str, args: Value| {
        let mut c = call(id, "dispatch", args);
        assert_eq!(l.before_approval(&mut c, &cx), CallFlow::Continue);
        assert!(c.args.get("background").is_none());
        let mut out = ToolOutcome::ok("started", "ok");
        l.after_call(&c, &cx, &mut out);
        out.content
    };
    let first = run("a", json!({"role": "researcher", "prompt": "p", "background": true}));
    assert!(first.ends_with("[args: ignored `background`: not a parameter of dispatch]"), "{first}");
    assert_eq!(run("b", json!({"role": "researcher", "prompt": "p", "background": false})), "started", "told once");
}
