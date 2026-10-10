//! Scholarly indexes: OpenAlex (works search), Crossref (the DOI authority) and Semantic
//! Scholar's keyless graph API. Each maps its structured identifiers into [`Identity`]; only the
//! namespace a channel is authoritative for can confirm a merge (ADR-036 §5).

use marlowe_proto::ChannelId;
use serde_json::Value;

use super::{clip, enc, get_json, limit, num, s, spec_of, SNIPPET_CHARS};
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, CitationDirection, CitationQuery};
use crate::source::{normalize_arxiv, Identity, Namespace, SourceRef};
use crate::xml::strip_tags;

/// An arXiv id from a landing-page URL (`arxiv.org/abs/...`), as indexes record preprints.
fn arxiv_from_url(url: &str) -> Option<String> {
    url.contains("arxiv.org/").then(|| normalize_arxiv(url)).flatten()
}

// ------------------------------------------------------------------ OpenAlex

const OPENALEX_SELECT: &str = "id,doi,display_name,publication_date,authorships,ids,abstract_inverted_index,\
primary_location,locations,is_retracted,cited_by_count,type,open_access";

/// OpenAlex's abstract comes as an inverted index (word -> positions); this rebuilds the text.
pub fn openalex_abstract(inv: &Value) -> String {
    let Some(map) = inv.as_object() else { return String::new() };
    let mut words: Vec<(u64, &str)> = Vec::new();
    for (w, positions) in map {
        for p in positions.as_array().into_iter().flatten().filter_map(Value::as_u64) {
            if p < 20_000 {
                words.push((p, w.as_str()));
            }
        }
    }
    words.sort_unstable_by_key(|(p, _)| *p);
    words.iter().map(|(_, w)| *w).collect::<Vec<_>>().join(" ")
}

/// One OpenAlex work as a route.
pub fn openalex_work(w: &Value) -> Option<SourceRef> {
    let title = s(w, "display_name").or_else(|| s(w, "title"))?;
    let ids = w.get("ids").cloned().unwrap_or(Value::Null);
    let mut identity = Identity::default();
    if let Some(id) = s(w, "id") {
        identity = identity.with_openalex(&id);
    }
    if let Some(d) = s(w, "doi") {
        identity = identity.with_doi(&d);
    }
    if let Some(p) = s(&ids, "pmid") {
        identity = identity.with_pmid(p.rsplit('/').next().unwrap_or(&p));
    }
    if let Some(p) = s(&ids, "pmcid") {
        identity = identity.with_pmcid(p.trim_end_matches('/').rsplit('/').next().unwrap_or(&p));
    }
    let primary = w.get("primary_location").cloned().unwrap_or(Value::Null);
    let landing = s(&primary, "landing_page_url");
    let locations = w.get("locations").and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(a) = locations.iter().filter_map(|l| s(l, "landing_page_url")).find_map(|u| arxiv_from_url(&u)) {
        identity = identity.with_arxiv(&a);
    }
    let url = landing
        .clone()
        .or_else(|| identity.doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .or_else(|| identity.openalex.as_ref().map(|o| format!("https://openalex.org/{o}")))?;
    let authors: Vec<String> = w
        .get("authorships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("author").and_then(|x| s(x, "display_name")))
        .collect();
    let venue = primary.get("source").and_then(|x| s(x, "display_name")).unwrap_or_default();
    let oa = w.get("open_access").and_then(|x| s(x, "oa_url")).unwrap_or_default();
    Some(
        SourceRef {
            identity: Some(identity),
            authors,
            date: s(w, "publication_date"),
            snippet: clip(&openalex_abstract(w.get("abstract_inverted_index").unwrap_or(&Value::Null)), SNIPPET_CHARS),
            retracted: w.get("is_retracted").and_then(Value::as_bool).unwrap_or(false),
            ..SourceRef::new(ChannelId::OpenAlex, &url, &title)
        }
        .with_meta("cited_by", num(w, "cited_by_count").unwrap_or_default())
        .with_meta("venue", venue)
        .with_meta("type", s(w, "type").unwrap_or_default())
        .with_meta("oa_url", oa),
    )
}

pub struct OpenAlexChannel {
    spec: ChannelSpec,
}

impl Default for OpenAlexChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAlexChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::OpenAlex) }
    }

    /// The works-list URL for a citation query: `cites:W…` (forward) or `cited_by:W…`
    /// (backward), most cited first. `None` without an OpenAlex id for the work.
    pub fn citations_url(&self, c: &CitationQuery, q: &ChannelQuery) -> Option<String> {
        let w = c.work.openalex.as_ref()?;
        let filter = match c.direction {
            CitationDirection::CitedBy => format!("cites:{w}"),
            CitationDirection::References => format!("cited_by:{w}"),
        };
        let mut u = format!(
            "{}/works?filter={filter}&sort=cited_by_count:desc&per_page={}&select={OPENALEX_SELECT}",
            self.spec.endpoint,
            limit(q).min(50)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!(",from_publication_date:{y}-01-01"));
        }
        Some(u)
    }

    pub fn search_url(&self, q: &ChannelQuery) -> String {
        let mut u = format!(
            "{}/works?search={}&per_page={}&select={OPENALEX_SELECT}",
            self.spec.endpoint,
            enc(q.text.trim()),
            limit(q).min(50)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&filter=from_publication_date:{y}-01-01"));
        }
        u
    }
}

impl Channel for OpenAlexChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let url = match &query.citations {
            Some(c) => self.citations_url(c, query).ok_or_else(|| ChannelError::Parse {
                channel: ChannelId::OpenAlex,
                detail: "a citation query needs the work's OpenAlex id".into(),
            })?,
            None => self.search_url(query),
        };
        let v = get_json(ctx, ChannelId::OpenAlex, url)?;
        let results = v
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| ChannelError::Parse { channel: ChannelId::OpenAlex, detail: "no `results` array".into() })?;
        Ok(results.iter().filter_map(openalex_work).collect())
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let Some(w) = (ns == Namespace::OpenAlex).then(|| Identity::default().with_openalex(id).openalex).flatten()
        else {
            return Err(ChannelError::NoLookup { channel: ChannelId::OpenAlex, namespace: ns });
        };
        let url = format!("{}/works/{w}?select={OPENALEX_SELECT}", self.spec.endpoint);
        match get_json(ctx, ChannelId::OpenAlex, url) {
            Ok(v) => Ok(openalex_work(&v)),
            Err(ChannelError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ------------------------------------------------------------------ Crossref

const CROSSREF_SELECT: &str =
    "DOI,title,author,issued,URL,abstract,container-title,type,is-referenced-by-count,update-to,subtitle";

/// `issued.date-parts[0]` as `YYYY[-MM[-DD]]`.
fn crossref_date(item: &Value) -> Option<String> {
    let parts = item.get("issued")?.get("date-parts")?.get(0)?.as_array()?;
    let n: Vec<u64> = parts.iter().filter_map(Value::as_u64).collect();
    match n.as_slice() {
        [y, m, d, ..] => Some(format!("{y:04}-{m:02}-{d:02}")),
        [y, m] => Some(format!("{y:04}-{m:02}")),
        [y] => Some(format!("{y:04}")),
        _ => None,
    }
}

/// One Crossref work as a route. A retraction notice says what it retracts; a work whose title
/// carries the publisher's `RETRACTED` stamp is marked retracted.
pub fn crossref_item(item: &Value) -> Option<SourceRef> {
    let doi = s(item, "DOI")?;
    let first = |k: &str| item.get(k).and_then(|t| t.get(0)).and_then(Value::as_str).map(str::trim).unwrap_or("");
    let mut title = first("title").to_string();
    if title.is_empty() {
        return None;
    }
    let sub = first("subtitle");
    if !sub.is_empty() {
        title = format!("{title}: {sub}");
    }
    let authors: Vec<String> = item
        .get("author")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| match (s(a, "given"), s(a, "family"), s(a, "name")) {
            (Some(g), Some(f), _) => Some(format!("{f}, {g}")),
            (None, Some(f), _) => Some(f),
            (_, _, Some(n)) => Some(n),
            _ => None,
        })
        .collect();
    let notice: Vec<String> = item
        .get("update-to")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|u| s(u, "type").is_some_and(|t| t.contains("retraction")))
        .filter_map(|u| s(u, "DOI"))
        .collect();
    let url = s(item, "URL").unwrap_or_else(|| format!("https://doi.org/{doi}"));
    let retracted = title.to_ascii_uppercase().starts_with("RETRACTED");
    Some(
        SourceRef {
            identity: Some(Identity::default().with_doi(&doi)),
            authors,
            date: crossref_date(item),
            snippet: clip(&strip_tags(&s(item, "abstract").unwrap_or_default()), SNIPPET_CHARS),
            retracted,
            ..SourceRef::new(ChannelId::Crossref, &url, &title)
        }
        .with_meta("venue", first("container-title"))
        .with_meta("type", s(item, "type").unwrap_or_default())
        .with_meta("cited_by", num(item, "is-referenced-by-count").unwrap_or_default())
        .with_meta("retraction_of", notice.join(" ")),
    )
}

pub struct CrossrefChannel {
    spec: ChannelSpec,
}

impl Default for CrossrefChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl CrossrefChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::Crossref) }
    }

    pub fn search_url(&self, q: &ChannelQuery) -> String {
        let mut u = format!(
            "{}/works?query={}&rows={}&select={CROSSREF_SELECT}",
            self.spec.endpoint,
            enc(q.text.trim()),
            limit(q).min(50)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&filter=from-pub-date:{y}"));
        }
        u
    }
}

impl Channel for CrossrefChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let v = get_json(ctx, ChannelId::Crossref, self.search_url(query))?;
        let items = v.get("message").and_then(|m| m.get("items")).and_then(Value::as_array).ok_or_else(|| {
            ChannelError::Parse { channel: ChannelId::Crossref, detail: "no `message.items` array".into() }
        })?;
        Ok(items.iter().filter_map(crossref_item).collect())
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let Some(doi) = (ns == Namespace::Doi).then(|| Identity::default().with_doi(id).doi).flatten() else {
            return Err(ChannelError::NoLookup { channel: ChannelId::Crossref, namespace: ns });
        };
        match get_json(ctx, ChannelId::Crossref, format!("{}/works/{}", self.spec.endpoint, enc(&doi))) {
            Ok(v) => Ok(v.get("message").and_then(crossref_item)),
            Err(ChannelError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ------------------------------------------------------------------ Semantic Scholar

const S2_FIELDS: &str =
    "title,abstract,year,publicationDate,authors,externalIds,url,venue,citationCount,openAccessPdf,publicationTypes";

/// One Semantic Scholar paper as a route.
pub fn s2_paper(p: &Value) -> Option<SourceRef> {
    let title = s(p, "title")?;
    let pid = s(p, "paperId")?;
    let ext = p.get("externalIds").cloned().unwrap_or(Value::Null);
    let mut identity = Identity::default().with_s2(&pid);
    if let Some(d) = s(&ext, "DOI") {
        identity = identity.with_doi(&d);
    }
    if let Some(a) = s(&ext, "ArXiv") {
        identity = identity.with_arxiv(&a);
    }
    if let Some(m) = num(&ext, "PubMed") {
        identity = identity.with_pmid(&m);
    }
    if let Some(m) = num(&ext, "PubMedCentral") {
        identity = identity.with_pmcid(&m);
    }
    let url = s(p, "url").unwrap_or_else(|| format!("https://www.semanticscholar.org/paper/{pid}"));
    let authors =
        p.get("authors").and_then(Value::as_array).into_iter().flatten().filter_map(|a| s(a, "name")).collect();
    let pdf = p.get("openAccessPdf").and_then(|o| s(o, "url")).unwrap_or_default();
    Some(
        SourceRef {
            identity: Some(identity),
            authors,
            date: s(p, "publicationDate").or_else(|| num(p, "year")),
            snippet: clip(&s(p, "abstract").unwrap_or_default(), SNIPPET_CHARS),
            ..SourceRef::new(ChannelId::SemanticScholar, &url, &title)
        }
        .with_meta("venue", s(p, "venue").unwrap_or_default())
        .with_meta("cited_by", num(p, "citationCount").unwrap_or_default())
        .with_meta("corpus_id", num(&ext, "CorpusId").unwrap_or_default())
        .with_meta("pdf", pdf),
    )
}

pub struct SemanticScholarChannel {
    spec: ChannelSpec,
}

impl Default for SemanticScholarChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticScholarChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::SemanticScholar) }
    }

    /// The paper key S2 accepts for a work: its S2 id, else `ARXIV:`, `DOI:` or `PMID:`.
    pub fn paper_key(work: &Identity) -> Option<String> {
        work.s2
            .clone()
            .or_else(|| work.arxiv_id.as_ref().map(|a| format!("ARXIV:{a}")))
            .or_else(|| work.doi.as_ref().map(|d| format!("DOI:{d}")))
            .or_else(|| work.pmid.as_ref().map(|p| format!("PMID:{p}")))
    }

    /// `/paper/{key}/citations` (forward) or `/references` (backward).
    pub fn citations_url(&self, c: &CitationQuery, q: &ChannelQuery) -> Option<String> {
        let key = Self::paper_key(&c.work)?;
        let edge = match c.direction {
            CitationDirection::CitedBy => "citations",
            CitationDirection::References => "references",
        };
        // `:` and `/` stay literal: S2 routes `DOI:10.18653/v1/N18-3011` as one path parameter.
        let key: String = key.split(['/', ':']).map(enc).collect::<Vec<_>>().join("/");
        let key = key.replacen('/', ":", 1);
        Some(format!("{}/paper/{key}/{edge}?limit={}&fields={S2_FIELDS}", self.spec.endpoint, limit(q).min(100)))
    }

    pub fn search_url(&self, q: &ChannelQuery) -> String {
        let mut u = format!(
            "{}/paper/search?query={}&limit={}&fields={S2_FIELDS}",
            self.spec.endpoint,
            enc(q.text.trim()),
            limit(q).min(100)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&year={y}-"));
        }
        u
    }
}

impl Channel for SemanticScholarChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        if let Some(c) = &query.citations {
            let url = self.citations_url(c, query).ok_or_else(|| ChannelError::Parse {
                channel: ChannelId::SemanticScholar,
                detail: "a citation query needs an S2, arXiv, DOI or PubMed id".into(),
            })?;
            let v = get_json(ctx, ChannelId::SemanticScholar, url)?;
            let side = if c.direction == CitationDirection::CitedBy { "citingPaper" } else { "citedPaper" };
            let data = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            let since = |r: &SourceRef| query.since_year.is_none_or(|y| r.year().is_none_or(|ry| ry >= y));
            return Ok(data.iter().filter_map(|e| e.get(side)).filter_map(s2_paper).filter(since).take(limit(query)).collect());
        }
        let v = get_json(ctx, ChannelId::SemanticScholar, self.search_url(query))?;
        // No match is `{"total": 0, ...}` without `data`.
        let data = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        if data.is_empty() && v.get("total").is_none() {
            return Err(ChannelError::Parse { channel: ChannelId::SemanticScholar, detail: "no `data` array".into() });
        }
        Ok(data.iter().filter_map(s2_paper).collect())
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let key = match ns {
            Namespace::S2 => id.trim().to_string(),
            _ => return Err(ChannelError::NoLookup { channel: ChannelId::SemanticScholar, namespace: ns }),
        };
        let url = format!("{}/paper/{}?fields={S2_FIELDS}", self.spec.endpoint, enc(&key));
        match get_json(ctx, ChannelId::SemanticScholar, url) {
            Ok(v) => Ok(s2_paper(&v)),
            Err(ChannelError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inverted_abstracts_rebuild_in_order() {
        let inv = json!({"rank": [1], "Low": [0], "adaptation": [2, 4], "is": [3]});
        assert_eq!(openalex_abstract(&inv), "Low rank adaptation is adaptation");
        assert_eq!(openalex_abstract(&Value::Null), "");
    }

    #[test]
    fn citation_queries_follow_the_graph_by_identity() {
        let work = Identity::default().with_arxiv("2301.00774v3").with_openalex("W4317553041");
        let fwd = ChannelQuery {
            since_year: Some(2025),
            ..ChannelQuery::citations(work.clone(), CitationDirection::CitedBy, 10)
        };
        let s2 = SemanticScholarChannel::new();
        let u = s2.citations_url(fwd.citations.as_ref().unwrap(), &fwd).unwrap();
        assert!(u.contains("/paper/ARXIV:2301.00774/citations?limit=10&fields="), "{u}");
        let doi = CitationQuery { work: Identity::default().with_doi("10.18653/v1/N18-3011"), direction: CitationDirection::CitedBy };
        let u = s2.citations_url(&doi, &fwd).unwrap();
        assert!(u.contains("/paper/DOI:10.18653/v1/n18-3011/citations"), "{u}");
        let back = ChannelQuery::citations(work.clone(), CitationDirection::References, 5);
        let u = s2.citations_url(back.citations.as_ref().unwrap(), &back).unwrap();
        assert!(u.contains("/references?limit=5"), "{u}");
        let oa = OpenAlexChannel::new();
        let u = oa.citations_url(fwd.citations.as_ref().unwrap(), &fwd).unwrap();
        assert!(u.contains("filter=cites:W4317553041&sort=cited_by_count:desc") && u.contains(",from_publication_date:2025-01-01"), "{u}");
        let u = oa.citations_url(back.citations.as_ref().unwrap(), &back).unwrap();
        assert!(u.contains("filter=cited_by:W4317553041"), "{u}");
        let bare = CitationQuery { work: Identity::default().with_url("https://x.org/p"), direction: CitationDirection::CitedBy };
        assert!(oa.citations_url(&bare, &back).is_none() && s2.citations_url(&bare, &back).is_none());
        // The S2 citations shape: each row wraps the paper.
        let row = json!({"citingPaper": {"paperId": "abc", "title": "OWL", "externalIds": {"ArXiv": "2310.05175"}}});
        let r = s2_paper(row.get("citingPaper").unwrap()).unwrap();
        assert_eq!(r.identity.unwrap().arxiv_id.as_deref(), Some("2310.05175"));
    }

    #[test]
    fn crossref_dates_and_retraction_notices() {
        let item = json!({
            "DOI": "10.1/ABC", "title": ["Retraction notice"], "issued": {"date-parts": [[2020, 3]]},
            "update-to": [{"type": "retraction", "DOI": "10.1/old"}], "author": [{"given": "Ada", "family": "Lovelace"}]
        });
        let r = crossref_item(&item).unwrap();
        assert_eq!(r.date.as_deref(), Some("2020-03"));
        assert_eq!(r.meta.get("retraction_of").map(String::as_str), Some("10.1/old"));
        assert_eq!(r.authors, ["Lovelace, Ada"]);
        assert!(!r.retracted);
        let stamped = json!({"DOI": "10.1/x", "title": ["RETRACTED: A result"]});
        assert!(crossref_item(&stamped).unwrap().retracted);
        assert!(crossref_item(&json!({"DOI": "10.1/x", "title": []})).is_none());
    }
}
