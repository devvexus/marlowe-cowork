//! The research phases: what `registry/phases/*.json` points at for a `research` project.
//!
//! The engine knows four step primitives; this module is the named functions behind the research
//! menu. [`register`] puts all of them in a [`Toolbox`]; [`ResearchDeps`] is what they need from
//! outside (channel search, the page reader and its network grant, the memory sink, where the
//! files go).
//!
//! | Phase | Step | Function | What it does |
//! |---|---|---|---|
//! | DataCollection | `vectors` | [`vectors`] | AttackVectors: one employee per question writes search angles; code adds date-worded variants |
//! | | `search` | [`collect`] | runs the queries on the keyless channels, deduplicates, takes the question's share of the quota |
//! | | `fetch` | [`collect`] | reads one hit under the run-scoped grant, numbers the links, stores the page |
//! | | `analyze` | [`analyze`] | one employee per page returns the AnalyzeOutput form; code fills index, source, urls, scores |
//! | DataValidation | `extract` | [`validate`] | quotes located verbatim in the stored page or the claim does not exist |
//! | | `condense` | [`validate`] | delta merge at 25/50/75/100%: similar claims become one condensed claim with its originals attached |
//! | | `verify` | [`validate`] | one employee per condensed claim re-reads the passages and returns a verdict |
//! | | `collect` | [`validate`] | the phase output: verified claims, the sources behind them, the records table, coverage |
//! | Thinking | `decide`, `route` | [`think`] | the lead decides report or keep researching; code raises a checkin when it should |
//! | Report | `sections`, `answer`, `cite`, `audit`, `assemble` | [`report`] | writers use verified claims only; every cited sentence is re-checked; code assembles `report.md` |
//! | Finish | `deliver`, `dispose`, `remember` | [`finish`] | the delivered copy, workspace disposal, extractive memories |
//!
//! Invariants these functions keep (design 4.9, 8.8): a model never writes a url, a source, an id
//! or a score; page text and everything a model wrote about it reaches a later prompt fenced as
//! data; a quote is verbatim or the claim is dropped; independence is counted by code; confidence
//! comes from code.

pub mod analyze;
pub mod collect;
pub mod credibility;
pub mod finish;
pub mod numbered;
pub mod report;
pub mod think;
pub mod validate;
pub mod vectors;
pub mod web;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::engine::Toolbox;
use crate::store::ProjectStore;

pub use finish::{MemoryEntry, MemorySink, NoMemory};
pub use web::{Admit, Failure, Fetched, Grant, PageFetch, PageRef, Resolver, SystemResolver, Web};

/// What the research functions need from outside.
pub struct ResearchDeps {
    /// The projects directory (the store's root).
    pub root: PathBuf,
    /// Where a finished project's deliverable is copied (`<home>\output`).
    pub output: PathBuf,
    /// Channel search.
    pub web: Arc<dyn Web>,
    /// The run-scoped network grant over the page reader.
    pub grant: Grant,
    /// Where extractive memories go.
    pub memory: Arc<dyn MemorySink>,
    /// Characters of a page one analysis reads at most.
    pub page_chars: usize,
    claimed: Mutex<HashSet<String>>,
    next_index: Mutex<HashMap<String, u32>>,
}

impl ResearchDeps {
    /// Dependencies over `root` (projects) and `output`, reading pages with `pages` under a grant
    /// that resolves host names with `resolver`.
    pub fn new(root: PathBuf, output: PathBuf, web: Arc<dyn Web>, pages: Arc<dyn PageFetch>, resolver: Arc<dyn Resolver>, memory: Arc<dyn MemorySink>) -> ResearchDeps {
        ResearchDeps {
            root,
            output,
            web,
            grant: Grant::new(pages, resolver),
            memory,
            page_chars: 14_000,
            claimed: Mutex::new(HashSet::new()),
            next_index: Mutex::new(HashMap::new()),
        }
    }

    /// A store over the projects directory (the files only; the index is not used here).
    pub fn store(&self) -> Result<ProjectStore, String> {
        ProjectStore::open_files(&self.root).map_err(|e| format!("the project files could not be opened: {e}"))
    }

    /// The stored text of a page, by hash.
    pub fn text(&self, project: &str, hash: &str) -> Option<String> {
        self.store().ok()?.get_source(project, hash).ok().flatten()
    }

    /// The link table of a stored page.
    pub fn links(&self, project: &str, hash: &str) -> Vec<marlowe_research::LinkRow> {
        self.store()
            .ok()
            .and_then(|s| s.get_source_aux(project, hash, "links").ok().flatten())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Takes every key in `keys` unless one of them is taken already; true when they were free.
    /// Atomic across tasks, so two questions searching at once cannot both pick the same source.
    pub(crate) fn claim(&self, keys: &[String]) -> bool {
        let mut set = self.claimed.lock().unwrap_or_else(|e| e.into_inner());
        if keys.iter().any(|k| set.contains(k)) {
            return false;
        }
        set.extend(keys.iter().cloned());
        true
    }

    /// Marks keys as taken without asking (what the state already holds).
    pub(crate) fn mark(&self, keys: impl IntoIterator<Item = String>) {
        self.claimed.lock().unwrap_or_else(|e| e.into_inner()).extend(keys);
    }

    /// The next source index of `project`, given the highest one the state holds. Atomic.
    pub(crate) fn next_source_index(&self, project: &str, in_state: u32) -> u32 {
        let mut m = self.next_index.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(project.to_string()).or_insert(0);
        *n = (*n).max(in_state) + 1;
        *n
    }
}

/// Registers every research function in `t`, replacing the stage B placeholders of the same
/// names. The generic functions (plan card, checkin answer, archive) stay.
pub fn register(t: &mut Toolbox, deps: &Arc<ResearchDeps>) {
    vectors::register(t, deps);
    collect::register(t, deps);
    analyze::register(t, deps);
    validate::register(t, deps);
    think::register(t, deps);
    report::register(t, deps);
    finish::register(t, deps);
}

/// A toolbox with the generic functions and the research functions.
pub fn toolbox(deps: &Arc<ResearchDeps>) -> Toolbox {
    let mut t = Toolbox::new();
    register(&mut t, deps);
    t
}

// ---- helpers the functions share --------------------------------------------------------------

/// `s` with url-shaped words removed (a model never writes a url), control characters turned to
/// spaces, whitespace collapsed and the result cut to `max` characters. The flag says whether a
/// url was removed.
pub(crate) fn plain(s: &str, max: usize) -> (String, bool) {
    let mut dropped = false;
    let kept: Vec<&str> = s
        .split_whitespace()
        .filter(|w| {
            let bad = marlowe_research::looks_like_url(w);
            dropped |= bad;
            !bad
        })
        .collect();
    let joined = kept.join(" ");
    (crate::engine::fence::clean(&joined, max, false), dropped)
}

/// [`plain`] without the flag.
pub(crate) fn plain_text(s: &str, max: usize) -> String {
    plain(s, max).0
}

/// The canonical form of a url for deciding whether two addresses are one page: the research
/// crate's canonical form with tracking parameters (`utm_*`, `fbclid`, ...) removed first.
pub(crate) fn canon(url: &str) -> Option<String> {
    let no_fragment = url.split('#').next().unwrap_or("");
    let rebuilt = match no_fragment.split_once('?') {
        Some((base, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|p| {
                    let k = p.split('=').next().unwrap_or("").to_ascii_lowercase();
                    !(k.starts_with("utm_") || matches!(k.as_str(), "utm" | "fbclid" | "gclid" | "yclid" | "mc_cid" | "mc_eid" | "igshid" | "ref" | "ref_src" | "ref_url"))
                })
                .collect();
            if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) }
        }
        None => no_fragment.to_string(),
    };
    marlowe_research::canonical_url(&rebuilt)
}

/// The host of a url, lower case, without `www.`.
pub(crate) fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    host.strip_prefix("www.").map(str::to_string).unwrap_or(host)
}

/// A string lower-cased with runs of whitespace collapsed (a record key, a name to compare).
pub(crate) fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The object inside a JSON value, or an empty one.
pub(crate) fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// A string field.
pub(crate) fn s<'a>(m: &'a Map<String, Value>, k: &str) -> &'a str {
    m.get(k).and_then(Value::as_str).unwrap_or("")
}

/// An unsigned number field.
pub(crate) fn u(m: &Map<String, Value>, k: &str) -> u64 {
    m.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// A real number field.
pub(crate) fn f(m: &Map<String, Value>, k: &str) -> f64 {
    m.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

/// The rows of a table field.
pub(crate) fn rows<'a>(m: &'a Map<String, Value>, k: &str) -> Vec<&'a Map<String, Value>> {
    m.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_object).collect()).unwrap_or_default()
}

/// A short hash of `parts` (`s_<8hex>` style ids for things the state has no item for).
pub(crate) fn short(prefix: &str, parts: &[&str]) -> String {
    crate::ids::derived_id(prefix, parts)
}

/// Counts of dropped things by reason, for a task's `rejected`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Rejected(BTreeMap<&'static str, u64>);

impl Rejected {
    pub(crate) fn add(&mut self, why: &'static str, n: u64) {
        if n > 0 {
            *self.0.entry(why).or_default() += n;
        }
    }

    pub(crate) fn into_value(self) -> Option<Value> {
        if self.0.is_empty() {
            None
        } else {
            Some(serde_json::to_value(self.0).unwrap_or(Value::Null))
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        serde_json::to_value(&self.0).unwrap_or(Value::Null)
    }
}

/// The question's id and text for a task: its `sub` and the plan's wording.
pub(crate) fn question_of(plan: &crate::plan::Plan, sub: &str) -> String {
    plan.questions.iter().find(|q| q.id == sub).map(|q| q.text.clone()).unwrap_or_else(|| plan.objective.clone())
}

/// The questions of every round, in order, as `(id, text)`: a continuation plan holds only its own
/// new questions, while the project's question items keep all of them. The lead and the writer
/// work from these, so a later round never hides what an earlier one found.
pub(crate) fn all_questions(st: &crate::state::ProjectState) -> Vec<(String, String)> {
    st.items_of_kind("question").into_iter().map(|q| (s(&q.data, "id").to_string(), s(&q.data, "text").to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_removes_urls_control_characters_and_cuts() {
        let (t, dropped) = plain("see https://evil.example/x?leak=1 and www.evil.example and the\u{1b}[31m text", 40);
        assert_eq!(t, "see and and the [31m text");
        assert!(dropped);
        let (t, dropped) = plain("no addresses here", 8);
        assert_eq!((t.as_str(), dropped), ("no addr…", false), "the ellipsis counts toward the cap, so a form's maxLength holds");
    }

    #[test]
    fn tracking_parameters_do_not_make_a_second_page() {
        assert_eq!(canon("https://www.example.org/a?utm_source=x&id=7#top"), canon("https://example.org/a?id=7"));
        assert_eq!(canon("https://example.org/a?fbclid=abc"), canon("https://example.org/a"));
        assert_ne!(canon("https://example.org/a?id=7"), canon("https://example.org/a?id=8"));
        assert_eq!(canon("ftp://example.org/a"), None);
    }

    #[test]
    fn hosts_and_names_normalise() {
        assert_eq!(host_of("https://www.Example.org/a?b=1"), "example.org");
        assert_eq!(norm("  Kimi   K2.6 "), "kimi k2.6");
    }

    #[test]
    fn source_indexes_count_on_from_the_state_and_never_repeat() {
        let deps = ResearchDeps::new(
            std::env::temp_dir(),
            std::env::temp_dir(),
            Arc::new(NoWeb),
            Arc::new(NoPages),
            Arc::new(SystemResolver),
            Arc::new(NoMemory),
        );
        assert_eq!(deps.next_source_index("p_1", 0), 1);
        assert_eq!(deps.next_source_index("p_1", 0), 2);
        assert_eq!(deps.next_source_index("p_1", 7), 8, "a restart that finds 7 sources in the state goes on from 8");
        assert_eq!(deps.next_source_index("p_2", 0), 1);
        assert!(deps.claim(&["a".into(), "b".into()]));
        assert!(!deps.claim(&["b".into(), "c".into()]), "one taken key refuses the whole set");
        assert!(deps.claim(&["c".into()]), "and the refused set took nothing");
    }

    pub(crate) struct NoWeb;
    impl Web for NoWeb {
        fn channels(&self) -> Vec<marlowe_research::ChannelId> {
            Vec::new()
        }
        fn search(&self, _c: marlowe_research::ChannelId, _q: &marlowe_research::ChannelQuery, _x: &marlowe_model::CancelToken) -> Result<Vec<marlowe_research::SourceRef>, marlowe_research::ChannelError> {
            Ok(Vec::new())
        }
    }
    pub(crate) struct NoPages;
    impl PageFetch for NoPages {
        fn read(&self, url: &str, _a: Admit<'_>, _c: &marlowe_model::CancelToken) -> Fetched {
            Fetched::failed(url, Failure::Unreachable, "no network in this build")
        }
    }
}
