//! The tool host that puts MCP servers beside a session's built-in tools.
//!
//! Model-visible: `mcp__<server>__<tool>` for every available tool that fits the host's MCP share
//! (compacted: first-sentence descriptions, no schema noise) or, past it, only `tool_search`, whose
//! description is a signature index of every MCP tool, and the tools it activated. And
//! `list_mcp_resources` / `read_mcp_resource` while a connected server has resources.
//! `tool_search` is for MCP tools only: the built-ins are all listed. A direct call to any
//! `mcp__server__tool` runs without loading it first; the forgiving layer validates it against the
//! server's real schema and answers a mistake with the exact signature.
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

use marlowe_agent::args::{arg_list, ArgRules};
use marlowe_agent::context::{tool_spec_tokens, PrefixBudget};
use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, Mutation, PersonaBlock, Profiles, ToolCallReq, ToolCtx, ToolGroup, ToolHost,
    ToolManifest, ToolMiddleware, ToolOutcome,
};
use marlowe_mcp::hub::{ExposedTool, Hub};
use marlowe_mcp::names::{self, GET_PROMPT, LIST_PROMPTS, LIST_RESOURCES, READ_RESOURCE, TOOL_SEARCH};
use marlowe_mcp::output;
use marlowe_mcp::McpError;
use marlowe_model::image::{base64_decode, ingest, BlobStore, STORE_LONG_EDGE};
use marlowe_model::{ResultPart, ToolSpec};
use serde_json::{json, Map, Value};

/// MCP tools beyond this many are behind `tool_search` whatever their size.
pub(super) const DEFER_OVER_TOOLS: usize = 20;

/// Tokens the layers above this host add to the tool list (the memory tools and `dispatch`).
const COMPOSED_RESERVE: usize = 600;
/// Tokens of `list_mcp_resources` and `read_mcp_resource`, listed while a server has resources.
const RESOURCE_SPECS: usize = 90;

pub struct McpTools {
    inner: Arc<dyn ToolHost>,
    hub: Arc<Hub>,
    /// Tools `tool_search` has loaded this session.
    activated: Mutex<BTreeSet<String>>,
    /// The conductor's context window, which sizes the tools slice this host shares.
    window: Option<u32>,
    /// Where an image a server returns is kept (`<home>lobs`); without one images are named
    /// in the text and not shown.
    blobs: Option<Arc<dyn BlobStore>>,
}

impl McpTools {
    pub fn new(inner: Arc<dyn ToolHost>, hub: Arc<Hub>, window: Option<u32>) -> Self {
        Self { inner, hub, activated: Mutex::new(BTreeSet::new()), window, blobs: None }
    }

    /// Keeps the images MCP tools return (a notebook's plot, a screenshot) as `Part::Image`.
    pub fn with_blobs(mut self, blobs: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Stores the server's images and attaches them to the outcome; one that cannot be decoded
    /// or stored is named in the text instead.
    pub(super) fn attach_images(&self, mut out: ToolOutcome, images: &[(String, String)]) -> ToolOutcome {
        let Some(store) = &self.blobs else { return out };
        for (i, (mime, data)) in images.iter().enumerate() {
            let stored = base64_decode(data).ok_or_else(|| "it is not valid base64".to_string()).and_then(|bytes| ingest(store.as_ref(), &bytes, STORE_LONG_EDGE).map_err(|e| e.to_string()));
            match stored {
                Ok(r) => out = out.with_part(ResultPart::Image(r)),
                Err(why) => out.content.push_str(&format!("
[image {} ({mime}) was not attached: {why}]", i + 1)),
            }
        }
        out
    }

    fn activated(&self) -> BTreeSet<String> {
        self.activated.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Tokens of the tools slice this host may fill: what the window's slice leaves after the
    /// built-ins it sits beside, the layers above it, and the resource tools.
    pub fn share(&self) -> usize {
        let slice = PrefixBudget::for_window(self.window.unwrap_or(32_768)).tools as usize;
        let built = tool_spec_tokens(&self.inner.specs()) as usize;
        let resources = if self.hub.has_resources() { RESOURCE_SPECS } else { 0 };
        slice.saturating_sub(built + COMPOSED_RESERVE + resources)
    }

    /// Whether the MCP tools are too many, or too large in their compact form, to list.
    pub fn deferred(&self, tools: &[ExposedTool]) -> bool {
        if tools.len() > DEFER_OVER_TOOLS {
            return true;
        }
        let compact: Vec<ToolSpec> = tools.iter().map(Self::compact_spec).collect();
        tool_spec_tokens(&compact) as usize > self.share()
    }

    fn is_internal(name: &str) -> bool {
        matches!(name, TOOL_SEARCH | LIST_RESOURCES | READ_RESOURCE | LIST_PROMPTS | GET_PROMPT)
    }

    fn handles(&self, name: &str) -> bool {
        names::is_mcp(name) || Self::is_internal(name)
    }

    /// The tool as its server declared it (without `$schema`): what calls are validated against
    /// and what `tool_search` loads.
    fn full_spec(t: &ExposedTool) -> ToolSpec {
        let mut schema = t.tool.input_schema.clone();
        if let Some(o) = schema.as_object_mut() {
            o.remove("$schema");
            o.entry("type").or_insert_with(|| json!("object"));
        }
        let description = if t.tool.description.is_empty() {
            format!("Tool {} of the MCP server {}.", t.tool.name, t.server)
        } else {
            t.tool.description.clone()
        };
        ToolSpec { name: t.exposed.clone(), description, input_schema: schema }
    }

    /// The listed form: schema noise dropped, property descriptions cut to their first clause
    /// (60 characters), the description to its first sentence (200). The name already carries the
    /// server, so no suffix says it again.
    fn compact_spec(t: &ExposedTool) -> ToolSpec {
        let full = Self::full_spec(t);
        ToolSpec {
            name: full.name,
            description: first_sentence(&full.description, 200),
            input_schema: compact_schema(&full.input_schema, false),
        }
    }

    /// `tool_search`: for tools of connected MCP servers only. Its description names the servers
    /// and carries a signature index of every MCP tool, so the model knows each by name and
    /// arguments without a schema. Past the share the index drops its description fragments, then
    /// groups by server, but never an argument name.
    fn search_spec(&self, tools: &[ExposedTool]) -> ToolSpec {
        let mut by_server: Vec<(String, usize)> = Vec::new();
        for t in tools {
            match by_server.iter_mut().find(|(s, _)| *s == t.server) {
                Some((_, n)) => *n += 1,
                None => by_server.push((t.server.clone(), 1)),
            }
        }
        let servers = by_server.iter().map(|(s, n)| format!("{s} ({n})")).collect::<Vec<_>>().join(", ");
        let head = format!(
            "Loads tools from the connected MCP servers: {servers}. They are not in the tool list; \
             call one directly by name (mcp__server__tool), or give keywords or `select:name1,name2` \
             to load its full schema. Tools:\n"
        );
        let share = self.share();
        let named = |t: &ExposedTool| format!("{}({})", t.exposed, arg_list(&Self::full_spec(t).input_schema));
        let level1: Vec<String> = tools
            .iter()
            .map(|t| {
                let gist = first_words(&t.tool.description, 8);
                if gist.is_empty() {
                    named(t)
                } else {
                    format!("{} — {gist}", named(t))
                }
            })
            .collect();
        let grouped = |names_only: bool| -> Vec<String> {
            by_server
                .iter()
                .map(|(server, _)| {
                    let mine: Vec<String> = tools
                        .iter()
                        .filter(|t| &t.server == server)
                        .map(|t| {
                            let args = arg_list(&Self::full_spec(t).input_schema);
                            let args = if names_only { arg_names(&args) } else { args };
                            format!("{}({args})", t.tool.name)
                        })
                        .collect();
                    format!("{server}: {}", mine.join(", "))
                })
                .collect()
        };
        let fits = |lines: &[String]| output::estimate_tokens(&format!("{head}{}", lines.join("\n"))) <= share;
        let index = if fits(&level1) {
            level1
        } else {
            let level2 = grouped(false);
            if fits(&level2) {
                level2
            } else {
                grouped(true)
            }
        };
        ToolSpec {
            name: TOOL_SEARCH.into(),
            description: format!("{head}{}", index.join("\n")),
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

    /// Answers `tool_search`: keywords or `select:` over the MCP tools; what it finds is callable at
    /// once and listed from now on.
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
            let spec = Self::full_spec(t);
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
                let images = out.images.clone();
                self.attach_images(Self::outcome(out.text, out.is_error, &note), &images)
            }
            Err(e) => Self::fail(e),
        }
    }
}

/// The first sentence of `s`, at most `max` characters.
fn first_sentence(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat.find(". ").map_or(flat.len(), |i| i + 1);
    cut(&flat[..end], max)
}

/// The first clause of `s`, at most `max` characters.
fn first_clause(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat.find(['.', ';', ':']).unwrap_or(flat.len());
    cut(flat[..end].trim_end(), max)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// The first `n` words of `s`.
fn first_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

/// `a, b?: int` -> `a, b`.
fn arg_names(args: &str) -> String {
    args.split(", ").map(|a| a.split(['?', ':']).next().unwrap_or(a)).collect::<Vec<_>>().join(", ")
}

/// A tool schema without `$schema`, `title`, `examples`, `default`, `readOnly`, and with property
/// descriptions cut to their first clause. `in_props`: `v` is a `properties` map, whose keys are
/// property names, not schema keywords.
fn compact_schema(v: &Value, in_props: bool) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                if !in_props && matches!(k.as_str(), "$schema" | "title" | "examples" | "example" | "default" | "readOnly") {
                    continue;
                }
                let new = match (k.as_str(), val) {
                    ("description", Value::String(d)) if !in_props => Value::String(first_clause(d, 60)),
                    ("properties", Value::Object(_)) if !in_props => compact_schema(val, true),
                    _ => compact_schema(val, false),
                };
                out.insert(k.clone(), new);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| compact_schema(x, false)).collect()),
        other => other.clone(),
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
        if !tools.is_empty() {
            if self.deferred(&tools) {
                specs.push(self.search_spec(&tools));
                let active = self.activated();
                specs.extend(tools.iter().filter(|t| active.contains(&t.exposed)).map(Self::full_spec));
            } else {
                specs.extend(tools.iter().map(Self::compact_spec));
            }
        }
        if self.hub.has_resources() {
            specs.extend(Self::resource_specs());
        }
        specs
    }

    /// An MCP tool is validated against what its server declared, listed or not.
    fn tool_spec(&self, name: &str) -> Option<ToolSpec> {
        if names::is_mcp(name) {
            return self.hub.find(name).map(|t| Self::full_spec(&t));
        }
        self.specs().into_iter().find(|s| s.name == name)
    }

    fn arg_rules(&self, name: &str) -> Option<ArgRules> {
        if self.handles(name) {
            marlowe_agent::args::wired_rules(name)
        } else {
            self.inner.arg_rules(name)
        }
    }

    fn prompt_hints(&self) -> Vec<String> {
        let mut hints = self.inner.prompt_hints();
        let tools = self.hub.tools();
        if !tools.is_empty() {
            hints.push(if self.deferred(&tools) {
                "Tools from MCP servers (mcp__server__tool) — call one directly by name, or tool_search to load its schema.".into()
            } else {
                "Tools from MCP servers (mcp__server__tool) are listed with the others: call them by name.".into()
            });
        }
        hints
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.call_scoped(call, &CallCtx::detached(ctx.clone()))
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if !self.handles(&call.name) {
            return self.inner.call_scoped(call, cx);
        }
        match call.name.as_str() {
            TOOL_SEARCH => self.tool_search_mcp(&call.args),
            LIST_RESOURCES => self.list_resources(&call.args),
            READ_RESOURCE => self.read_resource(&call.args, cx),
            LIST_PROMPTS => self.list_prompts(),
            GET_PROMPT => self.get_prompt(&call.args, cx),
            _ => self.call_mcp(call, cx),
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        let mut m = self.inner.middleware();
        m.push(Arc::new(super::ide_context::IdeContext::new(self.hub.clone())));
        m
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
