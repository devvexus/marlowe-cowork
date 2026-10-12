//! The web group (seam): web_fetch, arxiv, paper, web_search. Untrusted by construction: every
//! result taints the session, and after taint a new host (or any search) asks.

use marlowe_agent::{Consequence, Effects, Exposure, ToolCallReq, ToolGroup, ToolOutcome};
use marlowe_model::ToolSpec;
use serde_json::json;

use super::{describe_by, spec, ToolDef};
use crate::{first_line, research, research_input, str_arg, web, BuiltinTools};

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(web_fetch_spec(), ToolGroup::Web, Effects::NETWORK, |t, a, c| web::web_fetch(t, a, c))
            .consequence(fetch_consequence)
            .describe(|c| describe_by(c, "url"))
            .taints(always)
            .egress(fetch_host),
        ToolDef::new(arxiv_spec(), ToolGroup::Web, Effects::NETWORK, |t, a, c| research::arxiv(&t.research, a, &c.cancel))
            .consequence(|_, call, exposure| query_after_taint(call, exposure))
            .describe(|c| describe_by(c, "query"))
            .taints(always),
        ToolDef::new(paper_spec(), ToolGroup::Web, Effects::NETWORK, |t, a, c| research::paper(&t.research, a, &c.cancel))
            .consequence(paper_consequence)
            .describe(|c| describe_by(c, "url_or_arxiv_id"))
            .taints(always)
            .egress(|_, c| research::paper_host(research_input(&c.args))),
        ToolDef::new(web_search_spec(), ToolGroup::Web, Effects::NETWORK, |t, a, c| match &t.search {
            Some(b) => web::web_search(b.as_ref(), a, &c.cancel),
            None => ToolOutcome::error("web_search is not available in this build"),
        })
        .consequence(|_, call, exposure| query_after_taint(call, exposure))
        .describe(|c| describe_by(c, "query"))
        .taints(always)
        .enabled(|t| t.search.is_some()),
    ]
}

fn always(_tools: &BuiltinTools, _call: &ToolCallReq) -> bool {
    true
}

fn fetch_host(_tools: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
    let url = str_arg(&call.args, "url")?;
    marlowe_net::Target::parse(url).ok().map(|t| t.host)
}

/// After untrusted content, a host the user has not approved this session asks.
fn fetch_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    match fetch_host(tools, call).filter(|h| exposure.tainted && !exposure.approved_hosts.contains(h)) {
        Some(host) => Consequence::Ask { summary: format!("web_fetch {host}: a new host, after untrusted content") },
        None => Consequence::Free,
    }
}

fn paper_consequence(_tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    let host = research::paper_host(research_input(&call.args));
    match host.filter(|h| exposure.tainted && !exposure.approved_hosts.contains(h)) {
        Some(host) => Consequence::Ask { summary: format!("paper {host}: a new host, after untrusted content") },
        None => Consequence::Free,
    }
}

/// A search chosen after untrusted content asks (its query could carry what was read).
fn query_after_taint(call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    if !exposure.tainted {
        return Consequence::Free;
    }
    let query = first_line(str_arg(&call.args, "query").unwrap_or(""), 100);
    Consequence::Ask { summary: format!("{}: {} (after untrusted content)", call.name, query) }
}

fn web_fetch_spec() -> ToolSpec {
    spec(
        "web_fetch",
        "Fetch an https URL and return its main content as markdown (HTML, PDF, Office, text). \
         Honours robots.txt. Long documents come back in windows; continue from the character \
         `offset` given. With `prompt`, only its answer from the page comes back. A redirect to \
         another host is returned, not followed.",
        json!({
            "url": {"type": "string"},
            "prompt": {"type": "string"},
            "raw": {"type": "boolean"},
            "offset": {"type": "integer"}
        }),
        &["url"],
    )
}

fn arxiv_spec() -> ToolSpec {
    spec(
        "arxiv",
        "Search arXiv. Plain keywords, or field queries (`ti:`, `au:`, `abs:`, `cat:`), or an              arXiv id. Returns titles, authors, dates, ids and abstracts. Results are the authors'              claims, not findings; read the paper with `paper`.",
        json!({
            "query": {"type": "string"},
            "max": {"type": "integer", "description": "Results to return (default 8, at most 25)."}
        }),
        &["query"],
    )
}

fn paper_spec() -> ToolSpec {
    spec(
        "paper",
        "Read a paper from an https URL (HTML, PDF or LaTeX source) or an arXiv id (2301.01234; \
         arXiv's HTML is read first, then the source, then the PDF). Returns title, authors when \
         known, numbered sections, the abstract and the text as markdown; long papers come back \
         in windows, continue with `offset`. `part: \"references\"` returns the parsed reference \
         list with DOIs and arXiv ids.",
        json!({
            "url_or_arxiv_id": {"type": "string"},
            "offset": {"type": "integer", "description": "Character offset to continue from."},
            "part": {"type": "string", "enum": ["text", "references"], "description": "What to return (default text)."}
        }),
        &["url_or_arxiv_id"],
    )
}

fn web_search_spec() -> ToolSpec {
    spec(
        "web_search",
        "Search the web. Returns a short answer and the sources it cites, with URLs; use \
         `web_fetch` to read a source in full.",
        json!({
            "query": {"type": "string"},
            "allowed_domains": {"type": "array", "items": {"type": "string"}},
            "blocked_domains": {"type": "array", "items": {"type": "string"}}
        }),
        &["query"],
    )
}

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// This group's `persona/blocks/<group>.md` (`include_str!`) once it exists; appended only while
/// the group is exposed and has a live tool.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Per-host setup from the profile home: `web_fetch{prompt}` saves pages under its cache.
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(web::WebState { home: Some(setup.home.to_path_buf()) })
}

/// Whether a local model gets this group in `root`. Always exposed to local models (a core group).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
