//! The web group (seam): web_fetch (pages and papers), arxiv, web_search. Untrusted by construction: every
//! result taints the session, and after taint a new host (or any search) asks.

use marlowe_agent::{Consequence, Effects, Exposure, ToolCallReq, ToolGroup, ToolOutcome};
use marlowe_model::ToolSpec;

use super::{describe_by, ToolDef};
use crate::{first_line, research, research_input, str_arg, web, BuiltinTools};

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(web_fetch_spec(), ToolGroup::Web, Effects::NETWORK, |t, a, c| {
            if is_paper(a) {
                research::paper(&t.research, a, &c.cancel)
            } else {
                web::web_fetch(t, a, c)
            }
        })
            .consequence(fetch_consequence)
            .describe(|c| describe_by(c, "url"))
            .taints(always)
            .egress(fetch_host),
        ToolDef::new(arxiv_spec(), ToolGroup::Web, Effects::NETWORK, |t, a, c| research::arxiv(&t.research, a, &c.cancel))
            .consequence(|_, call, exposure| query_after_taint(call, exposure))
            .describe(|c| describe_by(c, "query"))
            .taints(always),
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

/// Whether a `web_fetch` is a paper read: an arXiv id or abs/pdf URL, openreview or doi.org, or any
/// call that sets `part` (the paper reader owns sections and references).
fn is_paper(args: &serde_json::Value) -> bool {
    if str_arg(args, "part").is_some() {
        return true;
    }
    let url = research_input(args);
    if research::arxiv_id(url).is_some() {
        return true;
    }
    marlowe_net::Target::parse(url)
        .ok()
        .is_some_and(|t| t.host == "doi.org" || t.host == "openreview.net" || t.host.ends_with(".openreview.net"))
}

fn fetch_host(_tools: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
    if is_paper(&call.args) {
        return research::paper_host(research_input(&call.args));
    }
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

/// A search chosen after untrusted content asks (its query could carry what was read).
fn query_after_taint(call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    if !exposure.tainted {
        return Consequence::Free;
    }
    let query = first_line(str_arg(&call.args, "query").unwrap_or(""), 100);
    Consequence::Ask { summary: format!("{}: {} (after untrusted content)", call.name, query) }
}

fn web_fetch_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("web_fetch")
}

fn arxiv_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("arxiv")
}

fn web_search_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("web_search")
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
