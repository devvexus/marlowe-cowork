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
    host_over(Arc::new(Inner), extra_tools, entry_extra, window)
}

fn host_over(inner: Arc<dyn ToolHost>, extra_tools: usize, entry_extra: Value, window: Option<u32>) -> (McpTools, FakeHttp) {
    let server = FakeHttp::start(HttpOpts { mcp: FakeOpts { extra_tools, ..Default::default() }, ..Default::default() });
    let mut entry = json!({"type": "http", "url": server.url()});
    entry.as_object_mut().unwrap().extend(entry_extra.as_object().unwrap().clone());
    let home = std::env::temp_dir().join(format!("marlowe-host-{}-{}", std::process::id(), marlowe_mcp::util::random_token(5)));
    std::fs::create_dir_all(&home).unwrap();
    let mut opts = HubOpts::new(Paths::new(&home, None), ConnectOpts::new(Arc::new(MemoryTokens::default())));
    opts.session = config::parse_session(&[json!({"mcpServers": {"web": entry}}).to_string()]).unwrap();
    let hub = Hub::start(opts);
    assert!(hub.wait_settled(Duration::from_secs(20)));
    (McpTools::new(inner, hub, window), server)
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

/// X20: `list_mcp_resources` lists a server's resources, `read_mcp_resource` returns one as text,
/// and what it returns is untrusted (the read taints the session).
#[test]
fn parity_x20_resources_are_listed_read_as_text_and_taint() {
    let (h, _s) = host(0, json!({}), None);
    let list = h.call_scoped(&call(LIST_RESOURCES, json!({})), &cx());
    assert!(!list.is_error, "{}", list.content);
    assert!(list.content.contains("file:///notes.txt") && list.content.contains("file:///data.json"), "{}", list.content);
    let read_call = call(READ_RESOURCE, json!({"server": "web", "uri": "file:///notes.txt"}));
    let read = h.call_scoped(&read_call, &cx());
    assert_eq!((read.content.as_str(), read.is_error), ("remember the milk", false));
    assert!(h.taints(&read_call), "a resource is server-written content");
}

#[test]
fn a_call_to_a_tool_that_is_gone_says_so() {
    let (h, _s) = host(0, json!({}), None);
    let out = h.call_scoped(&call("mcp__web__vanished", json!({})), &cx());
    assert!(out.is_error && out.content.contains("not an available MCP tool"), "{}", out.content);
}

/// A built-in host that defers its rarely used tools (T31) and lists its own `tool_search`.
struct DeferringInner;

impl ToolHost for DeferringInner {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec { name: "read".into(), description: "read".into(), input_schema: json!({"type": "object"}) },
            ToolSpec {
                name: TOOL_SEARCH.into(),
                description: "Load tools that are not listed up front. Deferred: monitor, todo_add.".into(),
                input_schema: json!({"type": "object"}),
            },
        ]
    }
    fn call(&self, call: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        if call.name == TOOL_SEARCH {
            let q = call.args["query"].as_str().unwrap_or("");
            return if q.contains("monitor") {
                ToolOutcome::ok("Loaded 1 tool(s); call them by these names:\n\n- monitor: watch a command's output", "1 tools loaded")
            } else {
                ToolOutcome::ok("No deferred tool matches.", "no match")
            };
        }
        ToolOutcome::ok(format!("inner {}", call.name), "ok")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

#[test]
fn parity_t31_built_in_and_mcp_deferral_share_one_tool_search_that_stays_in_place() {
    // Few MCP tools: the built-in host's tool_search is the only one, untouched, and a search
    // goes to it alone.
    let (h, _s) = host_over(Arc::new(DeferringInner), 0, json!({}), Some(200_000));
    let names: Vec<String> = h.specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names.iter().filter(|n| n.as_str() == TOOL_SEARCH).count(), 1, "{names:?}");
    let out = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "select:monitor"})), &cx());
    assert!(out.content.contains("- monitor: watch"), "{}", out.content);

    // Many MCP tools: one merged tool_search, at the built-in one's position, naming both.
    let (h, _s) = host_over(Arc::new(DeferringInner), 72, json!({"consequence": "inert"}), Some(32_768));
    let specs = h.specs();
    let searches: Vec<usize> = specs.iter().enumerate().filter(|(_, s)| s.name == TOOL_SEARCH).map(|(i, _)| i).collect();
    assert_eq!(searches, [1], "one tool_search, where the built-in host put it");
    assert!(specs[1].description.contains("web (80)") && specs[1].description.contains("Deferred: monitor, todo_add"), "{}", specs[1].description);

    // A query that only the built-in side matches gets its answer; one that only MCP matches
    // gets the MCP answer; one that both match gets both.
    let builtin = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "select:monitor"})), &cx());
    assert!(builtin.content.contains("- monitor: watch") && !builtin.is_error, "{}", builtin.content);
    let mcp = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "select:echo"})), &cx());
    assert!(mcp.content.contains("mcp__web__echo"), "{}", mcp.content);
    let both = h.call_scoped(&call(TOOL_SEARCH, json!({"query": "select:monitor,echo"})), &cx());
    assert!(both.content.contains("- monitor: watch") && both.content.contains("mcp__web__echo"), "{}", both.content);
}

#[test]
fn parity_u18_an_image_a_tool_returns_becomes_an_image_part_with_a_blob() {
    use marlowe_model::image::{BlobStore, MemoryBlobStore};
    use marlowe_model::ResultPart;

    let (h, _s) = host(0, json!({}), None);
    let store = Arc::new(MemoryBlobStore::new());
    let h = h.with_blobs(store.clone());
    let out = h.call_scoped(&call("mcp__web__image", json!({})), &cx());
    assert!(!out.is_error, "{}", out.content);
    let [ResultPart::Image(img)] = out.parts.as_slice() else { panic!("one image part: {:?} / {}", out.parts, out.content) };
    assert_eq!((img.mime.as_str(), img.w, img.h), ("image/png", 1, 1));
    assert!(store.get(&img.blob).is_ok(), "the bytes are in the blob store, the message carries the reference");
    assert!(out.content.contains("image/png image"), "the text still says there was a picture: {}", out.content);
}

#[test]
fn an_image_that_cannot_be_decoded_is_named_not_dropped_and_without_a_store_it_is_text_only() {
    use marlowe_model::image::MemoryBlobStore;

    let (h, _s) = host(0, json!({}), None);
    // No store: text only, no part.
    let out = h.call_scoped(&call("mcp__web__image", json!({})), &cx());
    assert!(out.parts.is_empty() && out.content.contains("image/png image"), "{out:?}");
    // A store, but the "picture" is not one.
    let h = h.with_blobs(Arc::new(MemoryBlobStore::new()));
    let bad = h.attach_images(ToolOutcome::ok("x", "y"), &[("image/png".into(), "bm90IGEgcGljdHVyZQ==".into()), ("image/png".into(), "!!!".into())]);
    assert!(bad.parts.is_empty());
    assert!(bad.content.contains("image 1 (image/png) was not attached") && bad.content.contains("image 2") && bad.content.contains("base64"), "{}", bad.content);
}
