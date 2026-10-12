//! The tool host that puts MCP servers beside a session's built-in tools.
//!
//! Model-visible: `mcp__<server>__<tool>` for every available tool (or, past the threshold,
//! only `tool_search` and the tools it activated), and `list_mcp_resources` /
//! `read_mcp_resource` while a connected server has resources.
//!
//! Callable but not listed, for the conductor's own commands (`@server:uri` mentions, the
//! `/mcp__server__prompt` slash commands): `mcp_list_prompts` and `mcp_get_prompt`. They go
//! through `ToolHost::call` like any tool, so no crate needs to depend on `marlowe-mcp`.
//!
//! ADR-052: what a server returns is untrusted, so every MCP call taints the conversation. A
//! tool loads with the consequence its server's config declares (default irreversible, so each
//! call asks).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, Mutation, PersonaBlock, Profiles, ToolCallReq, ToolCtx, ToolGroup, ToolHost,
    ToolManifest, ToolMiddleware, ToolOutcome,
};
use marlowe_mcp::hub::{ExposedTool, Hub};
use marlowe_mcp::names::{self, GET_PROMPT, LIST_PROMPTS, LIST_RESOURCES, READ_RESOURCE, TOOL_SEARCH};
use marlowe_mcp::output;
use marlowe_mcp::McpError;
use marlowe_model::ToolSpec;
use serde_json::{json, Map, Value};

/// MCP tools beyond this many are deferred behind `tool_search`.
pub(super) const DEFER_OVER_TOOLS: usize = 20;

/// Estimated tokens the MCP tool schemas may take before they are deferred, by context window.
pub(super) fn schema_budget(window: Option<u32>) -> usize {
    match window.unwrap_or(32_768) {
        0..=40_000 => 1_000,
        40_001..=130_000 => 3_000,
        _ => 8_000,
    }
}

pub struct McpTools {
    inner: Arc<dyn ToolHost>,
    hub: Arc<Hub>,
    /// Tools `tool_search` has loaded this session.
    activated: Mutex<BTreeSet<String>>,
    budget: usize,
}

impl McpTools {
    pub fn new(inner: Arc<dyn ToolHost>, hub: Arc<Hub>, window: Option<u32>) -> Self {
        Self { inner, hub, activated: Mutex::new(BTreeSet::new()), budget: schema_budget(window) }
    }

    fn activated(&self) -> BTreeSet<String> {
        self.activated.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the MCP tools are too many or too large to list.
    pub fn deferred(&self, tools: &[ExposedTool]) -> bool {
        if tools.len() > DEFER_OVER_TOOLS {
            return true;
        }
        let tokens: usize = tools.iter().map(|t| output::estimate_tokens(&format!("{}{}{}", t.exposed, t.tool.description, t.tool.input_schema))).sum();
        tokens > self.budget
    }

    fn is_internal(name: &str) -> bool {
        matches!(name, TOOL_SEARCH | LIST_RESOURCES | READ_RESOURCE | LIST_PROMPTS | GET_PROMPT)
    }

    fn handles(&self, name: &str) -> bool {
        names::is_mcp(name) || Self::is_internal(name)
    }

    fn spec_of(t: &ExposedTool) -> ToolSpec {
        let mut schema = t.tool.input_schema.clone();
        if let Some(o) = schema.as_object_mut() {
            o.remove("$schema");
            o.entry("type").or_insert_with(|| json!("object"));
        }
        let description = if t.tool.description.is_empty() {
            format!("Tool {} of the MCP server {}.", t.tool.name, t.server)
        } else {
            format!("{} (MCP server {})", t.tool.description, t.server)
        };
        ToolSpec { name: t.exposed.clone(), description, input_schema: schema }
    }

    fn search_spec(&self, tools: &[ExposedTool], builtin: Option<&ToolSpec>) -> ToolSpec {
        let mut by_server: Vec<(String, usize)> = Vec::new();
        for t in tools {
            match by_server.iter_mut().find(|(s, _)| *s == t.server) {
                Some((_, n)) => *n += 1,
                None => by_server.push((t.server.clone(), 1)),
            }
        }
        let servers = by_server.iter().map(|(s, n)| format!("{s} ({n})")).collect::<Vec<_>>().join(", ");
        // The built-ins deferred on a small window (T31) load through the same call.
        let builtin_note = builtin.map(|b| format!(" Also: {}", b.description)).unwrap_or_default();
        ToolSpec {
            name: TOOL_SEARCH.into(),
            description: format!(
                "Find and load tools from connected MCP servers: {servers}. Their schemas are not listed up front. \
                 Give keywords (\"create invoice\") or `select:name1,name2` for exact names. The tools found become \
                 callable at once; the result lists each with its parameters.{builtin_note}"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "keywords, or select:tool_name,tool_name"},
                    "limit": {"type": "integer", "description": "most tools to load (default 5)"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn resource_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: LIST_RESOURCES.into(),
                description: "List the resources (files, records, documents) that connected MCP servers offer, optionally for one server.".into(),
                input_schema: json!({"type": "object", "properties": {"server": {"type": "string"}}, "additionalProperties": false}),
            },
            ToolSpec {
                name: READ_RESOURCE.into(),
                description: "Read one MCP resource by server name and URI (from list_mcp_resources).".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"server": {"type": "string"}, "uri": {"type": "string"}},
                    "required": ["server", "uri"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    fn outcome(text: String, is_error: bool, note: &str) -> ToolOutcome {
        let first: String = text.lines().next().unwrap_or("").chars().take(80).collect();
        let summary = if note.is_empty() { first } else { note.to_string() };
        ToolOutcome { content: text, is_error, summary, parts: Vec::new() }
    }

    fn fail(e: McpError) -> ToolOutcome {
        ToolOutcome::error(e.to_string())
    }

    /// `tool_search` over the MCP tools and, when the built-in host defers its rarely used tools
    /// (a small window, T31), over those too.
    fn tool_search(&self, args: &Value, cx: &CallCtx) -> ToolOutcome {
        let inner_defers = self.inner.specs().iter().any(|s| s.name == TOOL_SEARCH);
        let tools = self.hub.tools();
        let mcp_defers = !tools.is_empty() && self.deferred(&tools);
        if !inner_defers {
            return self.tool_search_mcp(args);
        }
        let builtin = self.inner.call_scoped(&ToolCallReq { id: "tool_search".into(), name: TOOL_SEARCH.into(), args: args.clone() }, cx);
        if !mcp_defers {
            return builtin;
        }
        let mcp = self.tool_search_mcp(args);
        match (builtin.summary == "no match", mcp.summary == "no match") {
            (true, _) => mcp,
            (false, true) => builtin,
            (false, false) => ToolOutcome {
                content: format!("{}

{}", builtin.content, mcp.content),
                is_error: builtin.is_error && mcp.is_error,
                summary: format!("{}; {}", builtin.summary, mcp.summary),
                parts: Vec::new(),
            },
        }
    }

    fn tool_search_mcp(&self, args: &Value) -> ToolOutcome {
        let Some(query) = args.get("query").and_then(Value::as_str).filter(|q| !q.trim().is_empty()) else {
            return ToolOutcome::error("tool_search needs a `query`");
        };
        let limit = args.get("limit").and_then(Value::as_u64).map_or(5, |n| n.clamp(1, 20) as usize);
        let found = self.hub.search(query, limit);
        if found.is_empty() {
            let all = self.hub.tools();
            let servers = self.hub.counts();
            let mut list: Vec<String> = servers.iter().map(|(s, n)| format!("{s} ({n})")).collect();
            list.sort();
            return Self::outcome(
                format!("No MCP tool matches {query:?}. {} tools are available from: {}. Try other words, or select:name.", all.len(), list.join(", ")),
                false,
                "no match",
            );
        }
        let mut text = String::new();
        {
            let mut act = self.activated.lock().unwrap_or_else(|e| e.into_inner());
            for t in &found {
                act.insert(t.exposed.clone());
            }
        }
        text.push_str(&format!("Loaded {} tool(s); call them by these names:\n", found.len()));
        for t in &found {
            let spec = Self::spec_of(t);
            text.push_str(&format!("\n- {}: {}\n  parameters: {}\n", spec.name, spec.description, spec.input_schema));
        }
        Self::outcome(text, false, &format!("{} tools loaded", found.len()))
    }

    fn list_resources(&self, args: &Value) -> ToolOutcome {
        let only = args.get("server").and_then(Value::as_str);
        let mut lines = Vec::new();
        for (server, r) in self.hub.resources().into_iter().filter(|(s, _)| only.is_none_or(|o| o == s)) {
            let desc = if r.description.is_empty() { String::new() } else { format!(" - {}", r.description) };
            lines.push(format!("{server}  {}  {}{desc}", r.uri, r.name));
        }
        for (server, t) in self.hub.resource_templates().into_iter().filter(|(s, _)| only.is_none_or(|o| o == s)) {
            lines.push(format!("{server}  {} (template)  {}", t.uri_template, t.name));
        }
        if lines.is_empty() {
            return Self::outcome("No MCP resources are available.".into(), false, "none");
        }
        let n = lines.len();
        Self::outcome(lines.join("\n"), false, &format!("{n} resources"))
    }

    fn read_resource(&self, args: &Value, cx: &CallCtx) -> ToolOutcome {
        let (Some(server), Some(uri)) = (args.get("server").and_then(Value::as_str), args.get("uri").and_then(Value::as_str)) else {
            return ToolOutcome::error("read_mcp_resource needs `server` and `uri`");
        };
        match self.hub.read_resource(server, uri, &|| cx.cancel.is_cancelled()) {
            Ok(o) => Self::outcome(o.text.clone(), o.is_error, &format!("{} chars", o.text.chars().count())),
            Err(e) => Self::fail(e),
        }
    }

    fn list_prompts(&self) -> ToolOutcome {
        let mut lines = Vec::new();
        for (server, p) in self.hub.prompts() {
            let args = p.arguments.iter().map(|a| if a.required { a.name.clone() } else { format!("[{}]", a.name) }).collect::<Vec<_>>().join(" ");
            lines.push(format!("{server}\t{}\t{args}\t{}", p.name, p.description));
        }
        Self::outcome(lines.join("\n"), false, &format!("{} prompts", lines.len()))
    }

    fn get_prompt(&self, args: &Value, cx: &CallCtx) -> ToolOutcome {
        let (Some(server), Some(name)) = (args.get("server").and_then(Value::as_str), args.get("name").and_then(Value::as_str)) else {
            return ToolOutcome::error("mcp_get_prompt needs `server` and `name`");
        };
        let arguments: Map<String, Value> = args.get("arguments").and_then(Value::as_object).cloned().unwrap_or_default();
        match self.hub.get_prompt(server, name, &arguments, &|| cx.cancel.is_cancelled()) {
            Ok(msgs) => {
                let text = msgs.iter().map(|m| format!("{}: {}", m.role, m.text)).collect::<Vec<_>>().join("\n");
                Self::outcome(text, false, &format!("{} messages", msgs.len()))
            }
            Err(e) => Self::fail(e),
        }
    }

    fn call_mcp(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        match self.hub.call(&call.name, call.args.clone(), &|| cx.cancel.is_cancelled()) {
            Ok(out) => {
                // A tool the model called without loading it is loaded from now on.
                self.activated.lock().unwrap_or_else(|e| e.into_inner()).insert(call.name.clone());
                let note = if out.capped.truncated { format!("{} chars (truncated)", out.text.chars().count()) } else { String::new() };
                Self::outcome(out.text, out.is_error, &note)
            }
            Err(e) => Self::fail(e),
        }
    }
}

fn one_line(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max {
        s
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

impl ToolHost for McpTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        let tools = self.hub.tools();
        // A built-in host that defers its rarely used tools (T31) lists its own `tool_search`;
        // it stays where it is (what is loaded later is appended after it) and, when MCP tools
        // are deferred too, becomes the merged one.
        let builtin_search = specs.iter().position(|s| s.name == TOOL_SEARCH);
        if !tools.is_empty() {
            if self.deferred(&tools) {
                let merged = self.search_spec(&tools, builtin_search.map(|i| &specs[i]));
                match builtin_search {
                    Some(i) => specs[i] = merged,
                    None => specs.push(merged),
                }
                let active = self.activated();
                specs.extend(tools.iter().filter(|t| active.contains(&t.exposed)).map(Self::spec_of));
            } else {
                specs.extend(tools.iter().map(Self::spec_of));
            }
        }
        if self.hub.has_resources() {
            specs.extend(Self::resource_specs());
        }
        specs
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.call_scoped(call, &CallCtx::detached(ctx.clone()))
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if !self.handles(&call.name) {
            return self.inner.call_scoped(call, cx);
        }
        match call.name.as_str() {
            TOOL_SEARCH => self.tool_search(&call.args, cx),
            LIST_RESOURCES => self.list_resources(&call.args),
            READ_RESOURCE => self.read_resource(&call.args, cx),
            LIST_PROMPTS => self.list_prompts(),
            GET_PROMPT => self.get_prompt(&call.args, cx),
            _ => self.call_mcp(call, cx),
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        self.inner.middleware()
    }

    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        self.inner.persona_blocks()
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if !self.handles(&call.name) {
            return self.inner.consequence(call);
        }
        if Self::is_internal(&call.name) {
            return Consequence::Free;
        }
        match self.hub.find(&call.name) {
            Some(t) if !t.asks() => Consequence::Free,
            Some(t) => Consequence::Ask { summary: format!("{} ({}): {}", t.tool.name, t.server, one_line(&call.args, 120)) },
            None => Consequence::Ask { summary: format!("{}: {}", call.name, one_line(&call.args, 120)) },
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if self.handles(&call.name) {
            self.consequence(call)
        } else {
            self.inner.consequence_exposed(call, exposure)
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        match call.name.as_str() {
            // Everything a server wrote reaches the model untrusted: its tool output, but also the
            // descriptions and schemas `tool_search` loads and the names and descriptions the
            // listings return (ADR-052).
            n if names::is_mcp(n) || Self::is_internal(n) => true,
            _ => self.inner.taints(call),
        }
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        if self.handles(&call.name) {
            None
        } else {
            self.inner.egress_host(call)
        }
    }

    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        if !self.handles(name) {
            return self.inner.manifest(name);
        }
        let effects = if Self::is_internal(name) {
            Effects::READ
        } else {
            // A tool the server marked read-only is a read; anything else could do anything.
            match self.hub.find(name) {
                Some(t) if t.tool.read_only => Effects::READ,
                _ => Effects::RUN,
            }
        };
        Some(ToolManifest { group: ToolGroup::Mcp, profiles: Profiles::ALL, effects, path_args: &[] })
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        if self.handles(&call.name) {
            Mutation::None
        } else {
            self.inner.mutation(call, ctx)
        }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        if !self.handles(&call.name) {
            return self.inner.describe(call);
        }
        match call.name.as_str() {
            TOOL_SEARCH => format!("tool_search {}", call.args.get("query").and_then(Value::as_str).unwrap_or("")),
            READ_RESOURCE => format!("read resource {}", call.args.get("uri").and_then(Value::as_str).unwrap_or("")),
            LIST_RESOURCES => "list MCP resources".into(),
            n if names::is_mcp(n) => match self.hub.find(n) {
                Some(t) => format!("{}:{} {}", t.server, t.tool.name, one_line(&call.args, 60)),
                None => n.to_string(),
            },
            n => n.to_string(),
        }
    }
}
