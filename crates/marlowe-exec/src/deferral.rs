//! Rarely used built-in tools behind `tool_search` (T31) on a small context.
//!
//! A 32k model cannot spend 5.6k tokens of its prompt on tool schemas it will seldom call (gate
//! A15: the whole stable prefix is 9k). So on a window of [`DEFER_WINDOW`] tokens or less the host
//! lists the core tools and a `tool_search` tool, and leaves the [`DEFERRABLE`] ones out until the
//! model asks for them: `select:monitor`, or keywords. A loaded tool's schema is appended at the
//! tail of the tool list, so the bytes before it never change and the provider's prefix cache
//! holds. `tool_search`'s own description lists every deferrable name once, for the whole
//! session, so it does not change either. A call to a deferred tool by name still runs, and loads
//! it from then on. Larger windows list everything, as before.
//!
//! The daemon's MCP host wraps this host and merges its own `tool_search` (MCP tools) with this
//! one's.

use std::sync::Mutex;

use marlowe_model::ToolSpec;
use serde_json::{json, Value};

use crate::specs::{self, ToolDef};
use crate::BuiltinTools;

/// A window this size or smaller defers its rarely used tools.
pub const DEFER_WINDOW: u32 = 40_000;

/// The tool's name (shared with the MCP host's, which merges the two).
pub const TOOL_SEARCH: &str = "tool_search";

/// Built-ins a small-window session lists only after `tool_search` loads them.
pub const DEFERRABLE: &[&str] = &[
    "notebook_edit",
    "monitor",
    "task_list",
    "task_output",
    "task_stop",
    "cron_create",
    "cron_list",
    "cron_delete",
    "schedule_wakeup",
    "notify",
    "send_file",
    "notices",
    "list_agents",
    "send_message",
    "todo_add",
    "todo_update",
    "todo_get",
    "enter_plan",
    "enter_worktree",
    "exit_worktree",
    "workflow",
    "report_findings",
];

/// Tools loaded in this host's session, in the order they were loaded.
#[derive(Default)]
pub(crate) struct Loaded(Mutex<Vec<String>>);

impl Loaded {
    fn names(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn add(&self, name: &str) -> bool {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if g.iter().any(|n| n == name) {
            return false;
        }
        g.push(name.to_string());
        true
    }
}

fn is_deferrable(name: &str) -> bool {
    DEFERRABLE.contains(&name)
}

impl BuiltinTools {
    /// Whether this host defers its rarely used tools (a small context window).
    pub fn defers(&self) -> bool {
        self.defer
    }

    /// Defers (or not) the [`DEFERRABLE`] tools. [`BuiltinTools::with_model_exposure`] decides from
    /// the model's window.
    pub fn with_deferral(mut self, on: bool) -> Self {
        self.defer = on;
        self
    }

    /// The deferrable tools this host exposes, in table order.
    fn deferrable_defs(&self) -> Vec<&'static ToolDef> {
        specs::table().defs.iter().filter(|d| is_deferrable(d.name()) && self.exposes(d) && (d.enabled)(self)).collect()
    }

    /// The names of the tools that are deferred and not yet loaded.
    pub fn deferred_names(&self) -> Vec<String> {
        if !self.defer {
            return Vec::new();
        }
        let loaded = self.state::<Loaded>().names();
        self.deferrable_defs().iter().map(|d| d.name().to_string()).filter(|n| !loaded.contains(n)).collect()
    }

    /// `specs()` with the deferrable tools taken out and `tool_search` added, the loaded ones
    /// appended in load order.
    pub(crate) fn apply_deferral(&self, mut specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
        if !self.defer {
            return specs;
        }
        let present = self.deferrable_defs();
        if present.is_empty() {
            return specs;
        }
        let loaded = self.state::<Loaded>().names();
        let mut tail: Vec<ToolSpec> = Vec::new();
        for name in &loaded {
            if let Some(d) = present.iter().find(|d| d.name() == name) {
                tail.push(d.spec.clone());
            }
        }
        specs.retain(|s| !present.iter().any(|d| d.name() == s.name));
        specs.push(search_spec(&present.iter().map(|d| d.name()).collect::<Vec<_>>()));
        specs.extend(tail);
        specs
    }

    /// Marks `name` loaded when it is a deferrable tool called before `tool_search` loaded it.
    pub(crate) fn note_called(&self, name: &str) {
        if self.defer && is_deferrable(name) && self.deferrable_defs().iter().any(|d| d.name() == name) {
            self.state::<Loaded>().add(name);
        }
    }

    /// Answers a `tool_search` call over the deferred built-ins.
    pub(crate) fn tool_search(&self, args: &Value) -> marlowe_agent::ToolOutcome {
        let Some(query) = args.get("query").and_then(Value::as_str).map(str::trim).filter(|q| !q.is_empty()) else {
            return marlowe_agent::ToolOutcome::error("tool_search needs a `query`: keywords, or select:name1,name2");
        };
        let limit = args.get("limit").and_then(Value::as_u64).map_or(5, |n| n.clamp(1, 20) as usize);
        let present = self.deferrable_defs();
        let picked: Vec<&ToolDef> = match query.strip_prefix("select:") {
            Some(list) => list
                .split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .filter_map(|n| present.iter().copied().find(|d| d.name() == n))
                .collect(),
            None => rank(&present, query, limit),
        };
        if picked.is_empty() {
            let names: Vec<&str> = present.iter().map(|d| d.name()).collect();
            let mut out = marlowe_agent::ToolOutcome::ok(
                format!("No deferred tool matches {query:?}. The deferred tools are: {}. Try other words, or select:name.", names.join(", ")),
                "no match",
            );
            out.summary = "no match".into();
            return out;
        }
        let loaded = self.state::<Loaded>();
        let mut text = format!("Loaded {} tool(s); call them by these names:\n", picked.len());
        for d in &picked {
            loaded.add(d.name());
            text.push_str(&format!("\n- {}: {}\n  parameters: {}\n", d.name(), d.spec.description, d.spec.input_schema));
        }
        marlowe_agent::ToolOutcome::ok(text, format!("{} tools loaded", picked.len()))
    }
}

fn search_spec(names: &[&str]) -> ToolSpec {
    ToolSpec {
        name: TOOL_SEARCH.into(),
        description: format!(
            "Load tools that are not listed up front. Deferred: {}. Give `select:name1,name2` for exact names, or keywords \
             (\"watch a log\"). The tools found become callable at once; the result lists each with its parameters.",
            names.join(", ")
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

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// The tools whose name or description best match the query's words, best first.
fn rank<'a>(defs: &[&'a ToolDef], query: &str, limit: usize) -> Vec<&'a ToolDef> {
    let q = words(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, usize, &ToolDef)> = defs
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let name = words(d.name());
            let desc = words(&d.spec.description);
            let score: u32 = q
                .iter()
                .map(|w| {
                    if name.iter().any(|n| n == w) {
                        4
                    } else if name.iter().any(|n| n.contains(w.as_str()) || w.contains(n.as_str())) {
                        2
                    } else if desc.iter().any(|n| n == w) {
                        1
                    } else {
                        0
                    }
                })
                .sum();
            (score > 0).then_some((score, i, *d))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, _, d)| d).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use marlowe_agent::{CallCtx, SessionScope, ToolCallReq, ToolCtx, ToolHost};
    use marlowe_model::{CancelToken, ModelInfo, ProviderKind};
    use marlowe_proto::SessionId;

    use super::*;
    use crate::BuiltinTools;

    fn dusk() -> ModelInfo {
        ModelInfo { context_window: Some(32_768), ..ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super") }
    }

    fn host(model: &ModelInfo) -> BuiltinTools {
        specs::configure(BuiltinTools::new(None), &specs::HostSetup { home: Path::new("C:/home"), workspace: None }).with_model_exposure(model)
    }

    fn cx() -> CallCtx {
        CallCtx::new(ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false), SessionScope::default())
    }

    fn call(name: &str, args: Value) -> ToolCallReq {
        ToolCallReq { id: "c".into(), name: name.into(), args }
    }

    fn names(h: &BuiltinTools) -> Vec<String> {
        h.specs().into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn a_small_window_lists_the_core_tools_and_tool_search() {
        let h = host(&dusk());
        let n = names(&h);
        assert!(n.contains(&"tool_search".to_string()), "{n:?}");
        for deferred in ["todo_add", "todo_update", "todo_get", "enter_plan", "send_message", "enter_worktree", "exit_worktree", "notebook_edit"] {
            assert!(!n.contains(&deferred.to_string()), "{deferred} is listed: {n:?}");
        }
        for core in ["read", "write", "edit", "bash", "todo", "ask_user", "agent", "exit_plan", "skill"] {
            assert!(n.contains(&core.to_string()), "{core} missing: {n:?}");
        }
        let search = h.specs().into_iter().find(|s| s.name == "tool_search").unwrap();
        for d in ["todo_add", "enter_plan", "send_message"] {
            assert!(search.description.contains(d), "tool_search names {d}");
        }
    }

    #[test]
    fn parity_t31_a_big_window_lists_everything_and_has_no_tool_search() {
        let big = ModelInfo { context_window: Some(200_000), ..ModelInfo::bare(ProviderKind::OpenRouter, "frontier") };
        let n = names(&host(&big));
        assert!(!n.contains(&"tool_search".to_string()));
        for t in ["todo_add", "enter_plan", "send_message", "notebook_edit"] {
            assert!(n.contains(&t.to_string()), "{t}");
        }
    }

    #[test]
    fn parity_t31_select_loads_a_tool_at_the_tail_and_the_prefix_bytes_do_not_change() {
        let h = host(&dusk());
        let before = h.specs();
        let out = h.call_scoped(&call("tool_search", json!({"query": "select:todo_add,enter_plan"})), &cx());
        assert!(!out.is_error && out.content.contains("Loaded 2 tool(s)") && out.content.contains("parameters:"), "{}", out.content);
        let after = h.specs();
        assert_eq!(after.len(), before.len() + 2);
        assert_eq!(serde_json::to_string(&after[..before.len()]).unwrap(), serde_json::to_string(&before).unwrap(), "the bytes before the load are identical");
        assert_eq!(after[before.len()].name, "todo_add");
        assert_eq!(after[before.len() + 1].name, "enter_plan");
        // Loading again changes nothing.
        h.call_scoped(&call("tool_search", json!({"query": "select:todo_add"})), &cx());
        assert_eq!(h.specs().len(), after.len());
        assert!(!h.deferred_names().contains(&"todo_add".to_string()));
    }

    #[test]
    fn keywords_find_deferred_tools_and_a_miss_lists_what_exists() {
        let h = host(&dusk());
        let out = h.call_scoped(&call("tool_search", json!({"query": "plan mode before a risky change"})), &cx());
        assert!(out.content.contains("enter_plan"), "{}", out.content);
        let miss = h.call_scoped(&call("tool_search", json!({"query": "zzzz"})), &cx());
        assert_eq!(miss.summary, "no match");
        assert!(miss.content.contains("The deferred tools are:"), "{}", miss.content);
        assert!(h.call_scoped(&call("tool_search", json!({})), &cx()).is_error);
    }

    #[test]
    fn calling_a_deferred_tool_by_name_works_and_loads_it() {
        let h = host(&dusk());
        assert!(!names(&h).contains(&"todo_get".to_string()));
        let _ = h.call_scoped(&call("todo_get", json!({})), &cx());
        assert!(names(&h).contains(&"todo_get".to_string()), "loaded from the first call");
    }
}

#[cfg(test)]
mod measure {
    use super::*;
    use marlowe_agent::context::estimate_tokens;
    use marlowe_agent::ToolHost;
    use marlowe_model::{ModelInfo, ProviderKind};

    #[test]
    #[ignore = "prints the per-tool schema cost"]
    fn print_tool_costs() {
        let dusk = ModelInfo { context_window: Some(32_768), ..ModelInfo::bare(ProviderKind::Ollama, "d") };
        let h = specs::configure(BuiltinTools::new(None), &specs::HostSetup { home: std::path::Path::new("C:/home"), workspace: None })
            .with_model_exposure(&dusk);
        let mut total = 0;
        for s in h.specs() {
            let t = estimate_tokens(&format!("{}{}{}", s.name, s.description, s.input_schema));
            total += t;
            eprintln!("{:>5}  {}", t, s.name);
        }
        eprintln!("{total:>5}  TOTAL");
    }
}
