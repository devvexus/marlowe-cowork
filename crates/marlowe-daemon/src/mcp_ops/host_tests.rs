//! The MCP tool host against a fake streamable-HTTP server on loopback.

use std::sync::Arc;
use std::time::Duration;

use marlowe_agent::{CallCtx, Consequence, ToolCallReq, ToolCtx, ToolGroup, ToolHost, ToolOutcome};
use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::{self, Paths};
use marlowe_mcp::fake::{FakeHttp, FakeOpts, HttpOpts};
use marlowe_mcp::hub::{Hub, HubOpts};
use marlowe_mcp::names::{GET_PROMPT, LIST_PROMPTS, LIST_RESOURCES, READ_RESOURCE, TOOL_SEARCH};
use marlowe_mcp::oauth::MemoryTokens;
use marlowe_mcp::output;
use marlowe_model::{CancelToken, ToolSpec};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use super::host::{schema_budget, McpTools};

struct Inner;

impl ToolHost for Inner {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "read".into(), description: "read".into(), input_schema: json!({"type": "object"}) }]
    }
    fn call(&self, call: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok(format!("inner {}", call.name), "ok")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn cx() -> CallCtx {
    CallCtx::detached(ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false))
}

fn call(name: &str, args: Value) -> ToolCallReq {
    ToolCallReq { id: "c1".into(), name: name.into(), args }
}

fn host(extra_tools: usize, entry_extra: Value, window: Option<u32>) -> (McpTools, FakeHttp) {
    let server = FakeHttp::start(HttpOpts { mcp: FakeOpts { extra_tools, ..Default::default() }, ..Default::default() });
    let mut entry = json!({"type": "http", "url": server.url()});
    entry.as_object_mut().unwrap().extend(entry_extra.as_object().unwrap().clone());
    let home = std::env::temp_dir().join(format!("marlowe-host-{}-{}", std::process::id(), marlowe_mcp::util::random_token(5)));
    std::fs::create_dir_all(&home).unwrap();
    let mut opts = HubOpts::new(Paths::new(&home, None), ConnectOpts::new(Arc::new(MemoryTokens::default())));
    opts.session = config::parse_session(&[json!({"mcpServers": {"web": entry}}).to_string()]).unwrap();
    let hub = Hub::start(opts);
    assert!(hub.wait_settled(Duration::from_secs(20)));
    (McpTools::new(Arc::new(Inner), hub, window), server)
}

fn tokens_of(specs: &[ToolSpec]) -> usize {
    specs.iter().map(|s| output::estimate_tokens(&format!("{}{}{}", s.name, s.description, s.input_schema))).sum()
}

#[test]
fn a_few_tools_are_listed_beside_the_builtins_and_called() {
    let (h, _s) = host(0, json!({}), None);
    let specs = h.specs();
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names[0], "read", "built-ins first, so their order is stable");
    assert!(names.contains(&"mcp__web__echo") && names.contains(&"mcp__web__add"), "{names:?}");
    assert!(!names.contains(&TOOL_SEARCH), "no search while the tools fit");
    assert!(names.contains(&LIST_RESOURCES) && names.contains(&READ_RESOURCE), "the server has resources");
    assert!(!names.contains(&GET_PROMPT) && !names.contains(&LIST_PROMPTS), "prompt access is callable, not listed");

    let out = h.call_scoped(&call("mcp__web__echo", json!({"text": "hi"})), &cx());
    assert_eq!((out.content.as_str(), out.is_error), ("hi", false));
    let spec = specs.iter().find(|s| s.name == "mcp__web__echo").unwrap();
    assert!(spec.description.contains("(MCP server web)"));
    assert_eq!(spec.input_schema["required"], json!(["text"]));
    // Anything else is the built-ins'.
    assert_eq!(h.call_scoped(&call("read", json!({})), &cx()).content, "inner read");
    assert_eq!(h.consequence(&call("read", json!({}))), Consequence::Free);
}

#[test]
fn every_mcp_call_asks_by_default_and_a_declared_level_frees_it() {
    let (h, _s) = host(0, json!({}), None);
    assert!(matches!(h.consequence(&call("mcp__web__echo", json!({"text": "x"}))), Consequence::Ask { summary } | Consequence::AskEvery { summary } if summary.contains("echo") && summary.contains("web")));
    let (h, _s) = host(0, json!({"consequence": "inert", "consequences": {"fail": "irreversible"}}), None);
    assert_eq!(h.consequence(&call("mcp__web__echo", json!({}))), Consequence::Free);
    assert!(matches!(h.consequence(&call("mcp__web__fail", json!({}))), Consequence::Ask { .. } | Consequence::AskEvery { .. }), "a per-tool level beats the server's");
    assert_eq!(h.consequence(&call(TOOL_SEARCH, json!({}))), Consequence::Free);
}

#[test]
fn mcp_output_taints_and_its_manifest_is_not_a_read() {
    let (h, _s) = host(0, json!({}), None);
    assert!(h.taints(&call("mcp__web__echo", json!({}))), "ADR-052: what a server returns is untrusted");
    assert!(h.taints(&call(READ_RESOURCE, json!({}))));
    assert!(h.taints(&call(GET_PROMPT, json!({}))));
    // Descriptions and schemas are server-written text too: a prompt injection in one must not
    // arrive clean.
    for listing in [TOOL_SEARCH, LIST_RESOURCES, LIST_PROMPTS] {
        assert!(h.taints(&call(listing, json!({}))), "{listing} returns text a server wrote");
    }
    assert!(!h.taints(&call("read", json!({}))));
    let m = h.manifest("mcp__web__echo").unwrap();
    assert_eq!(m.group, ToolGroup::Mcp);
    assert!(m.effects.runs && m.effects.writes, "an unknown tool could do anything, so plan mode blocks it");
    assert!(h.manifest(READ_RESOURCE).unwrap().effects.reads);
}

#[test]
fn eighty_tools_fit_a_small_prefix_behind_tool_search() {
    let (h, _s) = host(72, json!({"consequence": "inert"}), Some(32_768));
    let specs = h.specs();
    let mcp: Vec<ToolSpec> = specs.iter().filter(|s| s.name != "read" && s.name != LIST_RESOURCES && s.name != READ_RESOURCE).cloned().collect();
    assert_eq!(mcp.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), [TOOL_SEARCH], "only the search tool is listed");
    assert!(tokens_of(&mcp) <= schema_budget(Some(32_768)), "{} tokens", tokens_of(&mcp));
    assert!(mcp[0].description.contains("web (80)"), "{}", mcp[0].description);

    // Finding tools loads them.
    let found = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "invoices record", "limit": 3})), &cx());
    assert!(!found.is_error && found.content.contains("Loaded 3 tool(s)") && found.content.contains("parameters:"), "{}", found.content);
    let after = h.specs();
    let loaded: Vec<&str> = after.iter().map(|s| s.name.as_str()).filter(|n| n.starts_with("mcp__web__tool_")).collect();
    assert_eq!(loaded.len(), 3, "{loaded:?}");

    // A loaded tool is callable; a named one can be loaded exactly.
    let sel = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "select:echo"})), &cx());
    assert!(sel.content.contains("mcp__web__echo"));
    assert_eq!(h.call_scoped(&call("mcp__web__echo", json!({"text": "z"})), &cx()).content, "z");
    let none = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "kubernetes"})), &cx());
    assert!(none.content.contains("No MCP tool matches") && none.content.contains("web (80)"), "{}", none.content);
    assert!(h.call_scoped(&call(TOOL_SEARCH, json!({})), &cx()).is_error);
}

#[test]
fn a_big_window_lists_up_to_the_threshold_then_defers() {
    let (h, _s) = host(10, json!({}), Some(200_000));
    assert!(h.specs().iter().any(|s| s.name == "mcp__web__tool_000"), "18 tools are listed in a large window");
    let (h, _s) = host(21, json!({}), Some(200_000));
    assert!(!h.specs().iter().any(|s| s.name == "mcp__web__tool_000"), "29 tools exceed the count threshold");
    assert!(h.specs().iter().any(|s| s.name == TOOL_SEARCH));
}

#[test]
fn resources_and_prompts_are_reachable_through_the_host() {
    let (h, _s) = host(0, json!({}), None);
    let list = h.call_scoped(&call(LIST_RESOURCES, json!({})), &cx());
    assert!(list.content.contains("web  file:///notes.txt") && list.content.contains("file:///{name}.md (template)"), "{}", list.content);
    let read = h.call_scoped(&call(READ_RESOURCE, json!({"server": "web", "uri": "file:///notes.txt"})), &cx());
    assert_eq!(read.content, "remember the milk");
    assert!(h.call_scoped(&call(READ_RESOURCE, json!({"server": "web", "uri": "file:///nope"})), &cx()).is_error);
    let prompts = h.call_scoped(&call(LIST_PROMPTS, json!({})), &cx());
    assert!(prompts.content.contains("web\tgreet\tname\tGreet someone"), "{}", prompts.content);
    let got = h.call_scoped(&call(GET_PROMPT, json!({"server": "web", "name": "greet", "arguments": {"name": "Matthew"}})), &cx());
    assert_eq!(got.content, "user: Say hello to Matthew.");
}

#[test]
fn a_call_to_a_tool_that_is_gone_says_so() {
    let (h, _s) = host(0, json!({}), None);
    let out = h.call_scoped(&call("mcp__web__vanished", json!({})), &cx());
    assert!(out.is_error && out.content.contains("not an available MCP tool"), "{}", out.content);
}
