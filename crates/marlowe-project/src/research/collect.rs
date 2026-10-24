//! SearchAndFetch: no model.
//!
//! `search_sources` runs one question's queries on the keyless channels, deduplicates by canonical
//! url and identity (a DOI, an arXiv id), and takes the question's share of the round's source
//! quota; each pick is a `hit` item. `fetch_page` reads one hit under the run-scoped network grant
//! ([`super::web::Grant`]): public https the run itself found, robots.txt and per-host spacing
//! behind the reader, the quota counted from the pages already in the state. The page text is
//! stored with numbered links (`[L12]`, [`super::numbered`]) and its link table beside it.
//!
//! Nothing a hostile page says travels from here: a failed or refused page is an item with a
//! harness-written reason, and the page text is only ever read by one analysis employee.

use std::collections::HashSet;
use std::sync::Arc;

use marlowe_research::{ChannelError, ChannelId, ChannelQuery, SourceRef};
use serde_json::{json, Value};

use super::credibility::{ceiling, origin_of};
use super::numbered::number_links;
use super::web::{channel_error_class, Failure, Fetched};
use super::{object, plain_text, s, u, ResearchDeps};
use crate::engine::{Finished, TaskInput, Toolbox};
use crate::state::{Item, ItemStatus};

/// Results asked of each channel per query.
const PER_QUERY: usize = 8;
/// A page with fewer characters than this has nothing to read.
pub const MIN_READABLE_CHARS: usize = 200;
/// A page with fewer characters than this is `thin`.
pub const THIN_CHARS: usize = 500;

/// Titles an extractor sometimes keeps for a page whose real title is elsewhere.
fn placeholder_title(t: &str) -> bool {
    let l = t.trim().to_lowercase();
    l.chars().count() < 4 || matches!(l.as_str(), "view pdf" | "pdf" | "download" | "download pdf" | "untitled" | "home" | "full text" | "abstract" | "read more")
}

/// The identity keys of a source, as strings (`doi:...`, `arxiv:...`).
fn identity_keys(r: &SourceRef) -> Vec<String> {
    r.identity.as_ref().map(|i| i.keys().into_iter().map(|(ns, v)| format!("{ns:?}:{v}").to_lowercase()).collect()).unwrap_or_default()
}

/// Whether the channel's own record carries an identity that confirms the work (a DOI, PMID or
/// arXiv id the channel returned, not one page text asserted).
fn confirmed(r: &SourceRef) -> bool {
    matches!(r.channel, ChannelId::Arxiv | ChannelId::OpenAlex | ChannelId::Crossref | ChannelId::SemanticScholar | ChannelId::Pubmed)
        && r.identity.as_ref().is_some_and(|i| i.doi.is_some() || i.arxiv_id.is_some() || i.pmid.is_some())
}

/// Round-robin interleave: every query's first result, then every query's second, and so on.
fn interleave(lists: Vec<Vec<SourceRef>>) -> Vec<SourceRef> {
    let mut out = Vec::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..longest {
        for l in &lists {
            if let Some(r) = l.get(rank) {
                out.push(r.clone());
            }
        }
    }
    out
}

/// What the state already holds that a search must not repeat.
struct Known {
    keys: HashSet<String>,
    round_hits: u32,
    seeded: bool,
}

fn known_in_state(input: &TaskInput) -> Result<Known, String> {
    let round = input.plan.plan_no as u64;
    input
        .project_handle
        .read(move |st| {
            let mut k = Known { keys: HashSet::new(), round_hits: 0, seeded: false };
            for it in st.items_of_kind("hit").into_iter().chain(st.items_of_kind("page")) {
                if let Some(c) = it.data.get("url").and_then(Value::as_str).and_then(super::canon) {
                    k.keys.insert(c);
                }
                if let Some(i) = it.data.get("identity").and_then(Value::as_str) {
                    k.keys.insert(i.to_lowercase());
                }
                if it.kind == "hit" && u(&it.data, "round") == round {
                    k.round_hits += 1;
                    k.seeded |= s(&it.data, "origin") == "seed";
                }
            }
            k
        })
        .map_err(|e| e.to_string())
}

fn hit_item(input: &TaskInput, r: &SourceRef, origin: &str, rank: usize) -> Item {
    let canon = super::canon(&r.url).unwrap_or_else(|| r.url.clone());
    let mut it = input.new_item("hit", "hit@1", &[&canon]);
    let title = plain_text(&r.title, 200);
    it.label = if title.is_empty() { super::host_of(&r.url) } else { title.clone() };
    // The first identifier that names the work (a DOI, an arXiv id), not its address.
    let identity = identity_keys(r).into_iter().find(|k| !k.starts_with("url:"));
    it.data = object(json!({
        "url": r.url.trim(),
        "title": if title.is_empty() { super::host_of(&r.url) } else { title },
        "channel": r.channel.as_str(),
        "date": r.date.as_deref().map(|d| plain_text(d, 32)),
        "snippet": plain_text(&r.snippet, 400),
        "origin": origin,
        "identity": identity,
        "identity_confirmed": confirmed(r),
        "retracted": r.retracted,
        "stars": r.meta.get("stars").and_then(|v| v.parse::<u64>().ok()),
        "rank": rank,
        "question": input.sub,
        "round": input.plan.plan_no,
    }));
    it
}

/// How many of a question's web queries are tried on the stand-in channels when the web has no
/// backend (they are rate limited; a few queries are enough to find the names to follow).
const STAND_IN_QUERIES: usize = 3;

/// Channels that stand in for a general web search that has no backend: discussion and news
/// (Hacker News), the encyclopedia, and repositories.
const STAND_INS: [ChannelId; 3] = [ChannelId::HackerNews, ChannelId::Wikipedia, ChannelId::Github];

/// Runs `q` on the stand-in channels that have not failed yet. A channel that fails is left out
/// for the rest of this search; only a cancel is an error.
fn stand_in_search(
    deps: &ResearchDeps,
    q: &ChannelQuery,
    cancel: &marlowe_model::CancelToken,
    failed: &mut Vec<ChannelId>,
    lists: &mut Vec<Vec<SourceRef>>,
) -> Result<(), String> {
    for ch in STAND_INS {
        if failed.contains(&ch) {
            continue;
        }
        match deps.web.search(ch, q, cancel) {
            Ok(rs) => lists.push(rs),
            Err(ChannelError::Cancelled) => return Err("cancelled".into()),
            Err(_) => failed.push(ch),
        }
    }
    Ok(())
}

fn search_sources(deps: &ResearchDeps, input: &TaskInput, cancel: &marlowe_model::CancelToken) -> Result<Finished, String> {
    let vector = input.item.as_ref().ok_or("there is no list of searches to run")?;
    let queries: Vec<(ChannelId, ChannelQuery)> = super::rows(&vector.data, "queries")
        .into_iter()
        .filter_map(|r| {
            let ch: ChannelId = s(r, "channel").parse().ok()?;
            let mut q = ChannelQuery::new(s(r, "query"), PER_QUERY);
            q.since_year = r.get("since_year").and_then(Value::as_u64).map(|y| y as u16);
            Some((ch, q))
        })
        .collect();
    let known = known_in_state(input)?;
    deps.mark(known.keys.iter().cloned());
    let mut out = Finished { tainted: true, ..Default::default() };
    let mut failed: Vec<ChannelId> = Vec::new();
    let mut lists: Vec<Vec<SourceRef>> = Vec::new();
    let mut stand_ins_run = 0usize;
    for (ch, q) in &queries {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if failed.contains(ch) {
            if *ch == ChannelId::Web && stand_ins_run < STAND_IN_QUERIES {
                stand_ins_run += 1;
                stand_in_search(deps, q, cancel, &mut failed, &mut lists)?;
            }
            continue;
        }
        match deps.web.search(*ch, q, cancel) {
            Ok(rs) => lists.push(rs),
            Err(ChannelError::Cancelled) => return Err("cancelled".into()),
            Err(e) => {
                failed.push(*ch);
                let mut note = input.new_item("note", "note@1", &["search", ch.as_str()]);
                let text = format!("{} search {} for \"{}\".", ch.label(), channel_error_class(&e), super::plain_text(&q.text, 60));
                note.label = text.clone();
                note.data = object(json!({ "text": text }));
                out.emits.push(note);
                // A general search with no backend still has the open channels that carry news.
                if *ch == ChannelId::Web && matches!(e, ChannelError::NoBackend { .. }) && stand_ins_run < STAND_IN_QUERIES {
                    stand_ins_run += 1;
                    stand_in_search(deps, q, cancel, &mut failed, &mut lists)?;
                }
            }
        }
    }

    // The question's share of the round's quota, and what is left of the round's total.
    let limits = &input.plan.limits;
    let questions = input.plan.questions.len().max(1) as u32;
    let share = limits.max_sources.div_ceil(questions);
    let mut room = limits.max_sources.saturating_sub(known.round_hits);
    let mut taken = 0u32;
    let mut rejected = super::Rejected::default();

    // Seeds (a continuation's flagged sources) are read first, once per round.
    if !input.plan.seed_sources.is_empty() && !known.seeded && deps.claim(&[format!("seeds:{}:{}", input.project, input.plan.plan_no)]) {
        for (n, url) in input.plan.seed_sources.iter().enumerate() {
            let Some(canon) = super::canon(url).filter(|_| url.trim_start().to_lowercase().starts_with("https://")) else {
                rejected.add("seed_not_https", 1);
                continue;
            };
            if room == 0 || !deps.claim(&[canon]) {
                continue;
            }
            let mut r = SourceRef::new(ChannelId::Web, url.trim(), "");
            r.title = super::host_of(url);
            deps.grant.found(&r.url);
            out.emits.push(hit_item(input, &r, "seed", n + 1));
            room -= 1;
        }
    }

    let mut seen_here: HashSet<String> = HashSet::new();
    for (rank, r) in interleave(lists).into_iter().enumerate() {
        if taken >= share || room == 0 {
            break;
        }
        if r.retracted {
            rejected.add("retracted", 1);
            continue;
        }
        let Some(canon) = super::canon(&r.url) else {
            rejected.add("no_usable_url", 1);
            continue;
        };
        if !r.url.trim_start().to_lowercase().starts_with("https://") {
            rejected.add("not_https", 1);
            continue;
        }
        let mut keys = identity_keys(&r);
        keys.push(canon.clone());
        if keys.iter().any(|k| seen_here.contains(k)) {
            rejected.add("duplicate", 1);
            continue;
        }
        seen_here.extend(keys.iter().cloned());
        if !deps.claim(&keys) {
            rejected.add("taken_by_another_question", 1);
            continue;
        }
        deps.grant.found(&r.url);
        out.emits.push(hit_item(input, &r, "search", rank + 1));
        taken += 1;
        room -= 1;
    }
    if taken == 0 && out.emits.iter().all(|e| e.kind != "hit") {
        let mut note = input.new_item("note", "note@1", &["search", "nothing"]);
        note.label = "No new source was found for this question.".into();
        note.data = object(json!({ "text": note.label }));
        out.emits.push(note);
    }
    out.rejected = rejected.into_value();
    Ok(out)
}

/// A page item for a read that produced no text: why, and whether it counts as failed or skipped.
fn unread(input: &TaskInput, hit: &Item, status: &str, kind: ItemStatus, why: &str) -> Item {
    let mut it = input.new_item("page", "page@1", &[&hit.id]);
    it.status = kind;
    it.error = Some(why.to_string());
    it.label = s(&hit.data, "title").to_string();
    it.data = object(json!({
        "source_id": super::short("s", &[&s(&hit.data, "url").to_string()]),
        "url": s(&hit.data, "url"),
        "title": s(&hit.data, "title"),
        "channel": s(&hit.data, "channel"),
        "status": status,
        "credibility": 0.0,
        "links": 0,
        "references": [],
        "question": input.sub,
        "identity_confirmed": false,
        "retracted": false,
        "chars": 0,
    }));
    it
}

fn fetch_page(deps: &ResearchDeps, input: &TaskInput, cancel: &marlowe_model::CancelToken) -> Result<Finished, String> {
    let hit = input.item.clone().ok_or("there is no search result to read")?;
    let url = s(&hit.data, "url").to_string();
    let pending = |status: &str, kind: ItemStatus, why: &str| -> Result<Finished, String> {
        Ok(Finished { emits: vec![unread(input, &hit, status, kind, why)], ..Default::default() })
    };

    // The quota left: the grant's quota less the pages already read. No grant, no reading.
    let (quota, used, dup_urls, dup_texts) = input
        .project_handle
        .read(|st| {
            let q = st.grants.net.as_ref().map(|n| n.quota);
            let mut used = 0u32;
            let mut urls = HashSet::new();
            let mut texts = HashSet::new();
            for p in st.items_of_kind("page") {
                if matches!(s(&p.data, "status"), "ok" | "thin" | "http" | "unreachable" | "unreadable" | "duplicate") {
                    used += 1;
                }
                if matches!(s(&p.data, "status"), "ok" | "thin") {
                    if let Some(c) = super::canon(s(&p.data, "url")) {
                        urls.insert(c);
                    }
                    texts.insert(s(&p.data, "text_ref").to_string());
                }
            }
            (q, used, urls, texts)
        })
        .map_err(|e| e.to_string())?;
    let Some(quota) = quota else {
        return pending("blocked", ItemStatus::Skipped, "the plan was not approved, so no page is read");
    };
    let remaining = quota.saturating_sub(used);
    if remaining == 0 {
        return pending("over_quota", ItemStatus::Skipped, "the project's fetch quota is used up");
    }

    let f: Fetched = deps.grant.read(&url, remaining, true, cancel);
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    bump_counters(input, &f);
    if let Some((kind, why)) = &f.failure {
        let (status, item) = match kind {
            Failure::Robots => ("robots", ItemStatus::Skipped),
            Failure::Blocked => ("blocked", ItemStatus::Skipped),
            Failure::Unreadable => ("unreadable", ItemStatus::Skipped),
            Failure::Http => ("http", ItemStatus::Failed),
            Failure::Unreachable => ("unreachable", ItemStatus::Failed),
        };
        return pending(status, item, why);
    }
    let final_url = if f.final_url.is_empty() { url.clone() } else { f.final_url.clone() };
    let chars = f.text.chars().count();
    if chars < MIN_READABLE_CHARS {
        return pending("unreadable", ItemStatus::Skipped, "the page has no readable text");
    }
    if super::canon(&final_url).is_some_and(|c| dup_urls.contains(&c) && c != super::canon(&url).unwrap_or_default()) {
        return pending("duplicate", ItemStatus::Skipped, "another source already read this page");
    }

    let numbered = number_links(&f.text, &final_url);
    let store = deps.store()?;
    let hash = store.put_source(&input.project, &numbered.text).map_err(|e| format!("the page could not be stored: {e}"))?;
    if dup_texts.contains(&hash) {
        return pending("duplicate", ItemStatus::Skipped, "another source already has this text");
    }
    let links_json = serde_json::to_string(&numbered.links).map_err(|e| e.to_string())?;
    store.put_source_aux(&input.project, &hash, "links", &links_json).map_err(|e| format!("the page's links could not be stored: {e}"))?;

    let meta = |k: &str| f.meta.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    let hit_title = s(&hit.data, "title");
    let title = [meta("citation_title"), f.title.as_deref(), Some(hit_title)]
        .into_iter()
        .flatten()
        .map(|t| plain_text(t, 200))
        .find(|t| !placeholder_title(t))
        .unwrap_or_else(|| super::host_of(&final_url));
    let date = meta("citation_publication_date").or_else(|| meta("citation_date")).or_else(|| meta("article:published_time")).map(|d| plain_text(d, 32)).or_else(|| hit.data.get("date").and_then(Value::as_str).map(str::to_string));
    let mut status = if chars < THIN_CHARS { "thin" } else { "ok" };
    if status == "ok" && chars < MIN_READABLE_CHARS {
        status = "thin";
    }
    let refs: Vec<Value> = f
        .references
        .iter()
        .filter(|r| r.doi.is_some() || r.arxiv_id.is_some() || r.url.is_some())
        .take(200)
        .map(|r| {
            json!({
                "doi": r.doi, "arxiv_id": r.arxiv_id,
                "url": r.url.as_deref().filter(|u| u.trim_start().to_lowercase().starts_with("https://")),
                "year": r.year,
                "label": r.label.as_deref().map(|l| plain_text(l, 120)).or_else(|| Some(plain_text(&r.text, 120))),
            })
        })
        .collect();
    let mut page = input.new_item("page", "page@1", &[&hit.id]);
    page.label = title.clone();
    let identity = hit.data.get("identity").and_then(Value::as_str).map(str::to_string);
    let key = identity.clone().or_else(|| super::canon(&final_url)).unwrap_or_else(|| final_url.clone());
    page.data = object(json!({
        "source_id": super::short("s", &[&key]),
        "url": final_url,
        "title": title,
        "channel": s(&hit.data, "channel"),
        "identity": identity,
        "identity_confirmed": hit.data.get("identity_confirmed").and_then(Value::as_bool).unwrap_or(false),
        "date": date,
        "status": status,
        "retracted": hit.data.get("retracted").and_then(Value::as_bool).unwrap_or(false),
        "stars": hit.data.get("stars").cloned().unwrap_or(Value::Null),
        "text_ref": hash,
        "chars": chars,
        "links": numbered.links.len(),
        "references": refs,
        "question": input.sub,
    }));
    let c = ceiling(&origin_of(&page.data));
    page.data.insert("credibility".into(), json!(c));
    Ok(Finished { emits: vec![page], tainted: true, ..Default::default() })
}

/// Fetch counts and the grant's `used`, kept for the screen. Not part of the journal: the quota
/// itself is counted from the page items, which are.
fn bump_counters(input: &TaskInput, f: &Fetched) {
    let ok = f.failure.is_none();
    let blocked = matches!(f.failure, Some((Failure::Blocked | Failure::Robots, _)));
    let _ = input.project_handle.send(crate::mutation::Mutation::AddFetch {
        ok: u32::from(ok),
        failed: u32::from(!ok && !blocked),
        blocked: u32::from(blocked),
        used: u32::from(!blocked),
    });
}

/// Registers `search_sources` and `fetch_page`.
pub fn register(t: &mut Toolbox, deps: &Arc<ResearchDeps>) {
    let d = deps.clone();
    t.code("search_sources", move |input, cancel| search_sources(&d, input, cancel));
    let d = deps.clone();
    t.code("fetch_page", move |input, cancel| fetch_page(&d, input, cancel));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_titles_are_recognised() {
        for t in ["View PDF", "pdf", " Download PDF ", "", "ab"] {
            assert!(placeholder_title(t), "{t:?}");
        }
        assert!(!placeholder_title("Kimi K2.6 technical report"));
    }

    #[test]
    fn interleave_alternates_queries_rank_by_rank() {
        let r = |u: &str| SourceRef::new(ChannelId::Web, u, u);
        let out = interleave(vec![vec![r("a1"), r("a2"), r("a3")], vec![r("b1")], vec![r("c1"), r("c2")]]);
        assert_eq!(out.iter().map(|x| x.url.as_str()).collect::<Vec<_>>(), ["a1", "b1", "c1", "a2", "c2", "a3"]);
    }

    #[test]
    fn identity_confirmation_needs_the_channels_own_record() {
        use marlowe_research::Identity;
        let mut r = SourceRef::new(ChannelId::OpenAlex, "https://doi.org/10.1/x", "t");
        assert!(!confirmed(&r));
        r.identity = Some(Identity::default().with_doi("10.1/x"));
        assert!(confirmed(&r));
        let mut web = SourceRef::new(ChannelId::Web, "https://example.org/p", "t");
        web.identity = Some(Identity::default().with_doi("10.1/x"));
        assert!(!confirmed(&web), "a web page that mentions a DOI has not confirmed it");
        assert_eq!(identity_keys(&r).len(), 1);
    }
}
