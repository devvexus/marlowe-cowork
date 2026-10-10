//! arXiv (export.arxiv.org Atom API): search, id lookup, and the Atom parser the exec `arxiv`
//! tool renders from. arXiv asks for one request per 3 s; the registry's limiter enforces it.

use marlowe_proto::ChannelId;

use super::{check_status, clip, get_raw, limit, spec_of, SNIPPET_CHARS};
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpRequest};
use crate::source::{normalize_arxiv, Identity, Namespace, SourceRef};
use crate::xml::{self, Node};

pub const ARXIV_API: &str = "https://export.arxiv.org/api/query";

/// One arXiv entry, as the API returned it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArxivEntry {
    /// With version: `2301.01234v2`, `cs/0601001v1`.
    pub id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub summary: String,
    pub published: String,
    pub updated: String,
    pub category: String,
    pub abs_url: String,
    pub pdf_url: String,
    /// The published version's DOI, when the authors recorded one.
    pub doi: Option<String>,
    pub comment: Option<String>,
    pub journal_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArxivFeed {
    pub total: Option<u64>,
    pub entries: Vec<ArxivEntry>,
    /// arXiv reports a malformed request as an entry titled "Error".
    pub error: Option<String>,
}

/// Parses an arXiv Atom response. Unparseable input gives an empty feed.
pub fn parse_atom(body: &str) -> ArxivFeed {
    let mut feed = ArxivFeed::default();
    let Ok(root) = xml::parse(body) else { return feed };
    let Some(top) = root.find("feed") else { return feed };
    feed.total = top.child_text("totalResults").and_then(|t| t.parse().ok());
    for entry in top.children_named("entry") {
        let title = entry.child_text("title").unwrap_or_default();
        let id_url = entry.child_text("id").unwrap_or_default();
        if id_url.contains("/api/errors") || title == "Error" {
            feed.error = Some(entry.child_text("summary").unwrap_or_default());
            continue;
        }
        feed.entries.push(entry_of(entry, &id_url, title));
    }
    feed
}

fn entry_of(entry: &Node, id_url: &str, title: String) -> ArxivEntry {
    let mut e = ArxivEntry {
        id: id_url.split_once("/abs/").map_or(id_url, |(_, id)| id).to_string(),
        title,
        summary: entry.child_text("summary").unwrap_or_default(),
        published: entry.child_text("published").unwrap_or_default(),
        updated: entry.child_text("updated").unwrap_or_default(),
        abs_url: id_url.replace("http://", "https://"),
        doi: entry.child_text("doi").filter(|d| !d.is_empty()),
        comment: entry.child_text("comment").filter(|d| !d.is_empty()),
        journal_ref: entry.child_text("journal_ref").filter(|d| !d.is_empty()),
        ..Default::default()
    };
    e.authors = entry.children_named("author").filter_map(|a| a.child_text("name")).collect();
    for link in entry.children_named("link") {
        let href = link.attr("href").unwrap_or_default().replace("http://", "https://");
        if link.attr("title") == Some("pdf") || link.attr("type") == Some("application/pdf") {
            e.pdf_url = href;
        }
    }
    if e.pdf_url.is_empty() && !e.id.is_empty() {
        e.pdf_url = format!("https://arxiv.org/pdf/{}", e.id);
    }
    e.category = entry.child("primary_category").and_then(|c| c.attr("term")).unwrap_or_default().to_string();
    e
}

/// arXiv's query encoding: `:` kept (field prefixes), space as `+`.
fn arxiv_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// An arXiv id with its version kept, from a bare id, the `arXiv:` form or an arxiv.org
/// abs/pdf/html URL. `None` when it is not one.
pub fn versioned_id(s: &str) -> Option<String> {
    let mut s = s.trim();
    if s.len() > 6 && s.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("arxiv:")) {
        s = s[6..].trim();
    }
    for pre in ["https://arxiv.org/", "http://arxiv.org/", "https://www.arxiv.org/", "https://export.arxiv.org/"] {
        if let Some(r) = s.strip_prefix(pre) {
            s = r.strip_prefix("abs/").or_else(|| r.strip_prefix("pdf/")).or_else(|| r.strip_prefix("html/"))?;
            s = s.split(['?', '#']).next().unwrap_or(s);
            s = s.strip_suffix(".pdf").unwrap_or(s);
            break;
        }
    }
    let digits =
        |t: &str, n: std::ops::RangeInclusive<usize>| n.contains(&t.len()) && t.bytes().all(|b| b.is_ascii_digit());
    let (base, version) = match s.rfind('v') {
        Some(i) if i > 0 && digits(&s[i + 1..], 1..=3) => (&s[..i], &s[i..]),
        _ => (s, ""),
    };
    let new_style = base.split_once('.').is_some_and(|(a, b)| digits(a, 4..=4) && digits(b, 4..=5));
    let old_style = base.split_once('/').is_some_and(|(arch, num)| {
        let (name, sub) = arch.split_once('.').unwrap_or((arch, ""));
        !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
            && (sub.is_empty() || (sub.len() == 2 && sub.bytes().all(|b| b.is_ascii_uppercase())))
            && digits(num, 7..=7)
    });
    (new_style || old_style).then(|| format!("{base}{version}"))
}

/// The request URL for a query; a bare arXiv id becomes an `id_list` lookup.
pub fn arxiv_url(query: &str, max: usize) -> String {
    let q = query.trim();
    if let Some(id) = versioned_id(q) {
        return format!("{ARXIV_API}?id_list={}&max_results=1", arxiv_encode(&id));
    }
    const FIELDS: [&str; 7] = ["all:", "ti:", "au:", "abs:", "cat:", "co:", "id:"];
    let search = if FIELDS.iter().any(|f| q.starts_with(f)) { q.to_string() } else { format!("all:{q}") };
    format!(
        "{ARXIV_API}?search_query={}&start=0&max_results={max}&sortBy=relevance&sortOrder=descending",
        arxiv_encode(&search)
    )
}

/// [`arxiv_url`] for a channel query, with `since_year` as a submission-date range.
pub fn search_url(q: &ChannelQuery) -> String {
    let text = q.text.trim();
    match q.since_year {
        Some(y) if versioned_id(text).is_none() => {
            let base = if ["all:", "ti:", "au:", "abs:", "cat:", "co:", "id:"].iter().any(|f| text.starts_with(f)) {
                text.to_string()
            } else {
                format!("all:{text}")
            };
            arxiv_url(&format!("({base}) AND submittedDate:[{y}01010000 TO 300001010000]"), limit(q))
        }
        _ => arxiv_url(text, limit(q)),
    }
}

/// One entry as a route: versionless abs URL, the arXiv id (authoritative) and any DOI.
pub fn to_source(e: &ArxivEntry) -> SourceRef {
    let base = normalize_arxiv(&e.id).unwrap_or_else(|| e.id.clone());
    let mut identity = Identity::default().with_arxiv(&e.id);
    if let Some(d) = &e.doi {
        identity = identity.with_doi(d);
    }
    let version = e.id.strip_prefix(base.as_str()).unwrap_or("");
    SourceRef {
        identity: Some(identity),
        authors: e.authors.clone(),
        date: (!e.published.is_empty()).then(|| e.published.clone()),
        snippet: clip(&e.summary, SNIPPET_CHARS),
        ..SourceRef::new(ChannelId::Arxiv, &format!("https://arxiv.org/abs/{base}"), &e.title)
    }
    .with_meta("version", version)
    .with_meta("category", e.category.clone())
    .with_meta("pdf", e.pdf_url.clone())
    .with_meta("updated", e.updated.clone())
    .with_meta("comment", e.comment.clone().unwrap_or_default())
    .with_meta("journal_ref", e.journal_ref.clone().unwrap_or_default())
}

/// The arXiv channel.
pub struct ArxivChannel {
    spec: ChannelSpec,
}

impl Default for ArxivChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl ArxivChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::Arxiv) }
    }

    fn feed(&self, url: String, ctx: &ChannelCtx<'_>) -> Result<ArxivFeed, ChannelError> {
        let id = ChannelId::Arxiv;
        let r = check_status(id, get_raw(ctx, id, HttpRequest::get(url).accept("application/atom+xml"))?)?;
        let body = String::from_utf8_lossy(&r.bytes);
        if !body.contains("<feed") {
            return Err(ChannelError::Parse { channel: id, detail: "not an Atom feed".into() });
        }
        let feed = parse_atom(&body);
        match feed.error {
            Some(e) => Err(ChannelError::Parse { channel: id, detail: format!("arXiv rejected the query: {e}") }),
            None => Ok(feed),
        }
    }
}

impl Channel for ArxivChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        Ok(self.feed(search_url(query), ctx)?.entries.iter().map(to_source).collect())
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let Some(id) = (ns == Namespace::Arxiv).then(|| versioned_id(id)).flatten() else {
            return Err(ChannelError::NoLookup { channel: ChannelId::Arxiv, namespace: ns });
        };
        Ok(self.feed(arxiv_url(&id, 1), ctx)?.entries.first().map(to_source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_keep_versions_and_reject_lookalikes() {
        assert_eq!(versioned_id("2301.01234").as_deref(), Some("2301.01234"));
        assert_eq!(versioned_id("arXiv:2301.01234v2").as_deref(), Some("2301.01234v2"));
        assert_eq!(versioned_id("https://arxiv.org/abs/2301.01234v2").as_deref(), Some("2301.01234v2"));
        assert_eq!(versioned_id("https://arxiv.org/pdf/2301.01234.pdf").as_deref(), Some("2301.01234"));
        assert_eq!(versioned_id("https://arxiv.org/html/2301.01234v1").as_deref(), Some("2301.01234v1"));
        assert_eq!(versioned_id("cs/0601001").as_deref(), Some("cs/0601001"));
        assert_eq!(versioned_id("math.GT/0309136v1").as_deref(), Some("math.GT/0309136v1"));
        assert_eq!(versioned_id("https://example.com/2301.01234"), None);
        assert_eq!(versioned_id("attention is all you need"), None);
        assert_eq!(versioned_id("2301.123"), None);
        // Found live: a query starting with a multi-byte character inside the first six bytes.
        assert_eq!(versioned_id("2025–2026 pruning methods 20–30B"), None);
        assert_eq!(versioned_id("éàü arXiv"), None);
    }

    #[test]
    fn urls_encode_fields_and_dates() {
        assert!(arxiv_url("attention", 5).contains("search_query=all:attention&"));
        assert!(arxiv_url("au:del_maestro AND ti:checkerboard", 5).contains("au:del_maestro+AND+ti:checkerboard"));
        assert!(arxiv_url("2301.01234", 5).contains("id_list=2301.01234"));
        let q = ChannelQuery { since_year: Some(2023), ..ChannelQuery::new("lora rank", 4) };
        let u = search_url(&q);
        assert!(u.contains("%28all:lora+rank%29+AND+submittedDate:%5B202301010000+TO+300001010000%5D"), "{u}");
        assert!(u.contains("max_results=4"));
    }
}
