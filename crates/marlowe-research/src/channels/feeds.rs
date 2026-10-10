//! RSS 2.0 / RSS 1.0 / Atom feeds. A query that is a feed (or page) URL reads that feed; any
//! other query searches the configured feeds' items by its words. Feeds are fetched through a
//! [`HostGate`] (robots.txt, per-host spacing) and cached per feed, so many queries cost one
//! fetch per feed per TTL. A page URL is followed to its advertised feed once (autodiscovery).

use std::time::Duration;

use marlowe_proto::ChannelId;

use super::{check_status, clip, get_raw, limit, spec_of, SNIPPET_CHARS};
use crate::cache::CacheKey;
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpRequest};
use crate::politeness::{HostGate, Refusal};
use crate::source::SourceRef;
use crate::xml::{self, strip_tags, Node};

/// Feeds searched when none are configured: research and engineering sources Matthew reads.
/// Each was checked live (robots.txt allows it and it parses) when added.
pub const DEFAULT_FEEDS: [&str; 5] = [
    "https://huggingface.co/blog/feed.xml",
    "https://blog.rust-lang.org/feed.xml",
    "https://simonwillison.net/atom/everything/",
    "https://lwn.net/headlines/rss",
    "https://bair.berkeley.edu/blog/feed.xml",
];

/// Per-feed cache lifetime.
const FEED_TTL: Duration = Duration::from_secs(3600);
const FEED_ACCEPT: &str = "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.1";

/// One feed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub title: String,
    pub link: String,
    pub summary: String,
    pub date: Option<String>,
    pub authors: Vec<String>,
}

/// `Tue, 10 Jun 2003 04:00:00 GMT` -> `2003-06-10`; ISO-8601 dates keep their first ten chars.
pub fn feed_date(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.len() >= 10 && t.as_bytes()[4] == b'-' && t[..4].bytes().all(|b| b.is_ascii_digit()) {
        return Some(t[..10].to_string());
    }
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let (mut d, mut m, mut y) = (None, None, None);
    for w in t.split([' ', ',']).filter(|w| !w.is_empty()) {
        let lw = w.to_ascii_lowercase();
        if let Some(i) = MONTHS.iter().position(|x| lw.starts_with(x)) {
            m = Some(i + 1);
        } else if w.len() == 4 && w.bytes().all(|b| b.is_ascii_digit()) {
            y = Some(w.to_string());
        } else if w.len() <= 2 && w.bytes().all(|b| b.is_ascii_digit()) && d.is_none() {
            d = w.parse::<u32>().ok();
        }
    }
    Some(match (y?, m, d) {
        (y, Some(m), Some(d)) => format!("{y}-{m:02}-{d:02}"),
        (y, Some(m), None) => format!("{y}-{m:02}"),
        (y, _, _) => y,
    })
}

fn atom_link(e: &Node) -> String {
    let links: Vec<&Node> = e.children_named("link").collect();
    links
        .iter()
        .find(|l| l.attr("rel").is_none_or(|r| r == "alternate"))
        .or(links.first())
        .and_then(|l| l.attr("href").map(str::to_string).or_else(|| Some(l.clean_text())))
        .unwrap_or_default()
}

/// Items of an RSS 2.0, RSS 1.0 (RDF) or Atom document. `None` when it is not a feed.
pub fn parse_feed(body: &str) -> Option<Vec<FeedItem>> {
    let root = xml::parse(body).ok()?;
    let top = root.children.iter().find(|c| matches!(c.name.as_str(), "rss" | "feed" | "RDF"))?;
    let mut out = Vec::new();
    if top.name == "feed" {
        for e in top.children_named("entry") {
            let summary = e.child("summary").or_else(|| e.child("content")).map(|n| strip_tags(&n.text_deep()));
            out.push(FeedItem {
                title: strip_tags(&e.child_text("title").unwrap_or_default()),
                link: atom_link(e),
                summary: summary.unwrap_or_default(),
                date: e.child_text("published").or_else(|| e.child_text("updated")).and_then(|d| feed_date(&d)),
                authors: e.children_named("author").filter_map(|a| a.child_text("name")).collect(),
            });
        }
    } else {
        for i in top.find_all("item") {
            let summary = i.child("description").or_else(|| i.child("encoded")).map(|n| strip_tags(&n.text_deep()));
            let author = i.child_text("creator").or_else(|| i.child_text("author"));
            out.push(FeedItem {
                title: strip_tags(&i.child_text("title").unwrap_or_default()),
                link: i
                    .child_text("link")
                    .filter(|l| !l.is_empty())
                    .or_else(|| i.child_text("guid"))
                    .unwrap_or_default(),
                summary: summary.unwrap_or_default(),
                date: i.child_text("pubDate").or_else(|| i.child_text("date")).and_then(|d| feed_date(&d)),
                authors: author.into_iter().filter(|a| !a.is_empty()).collect(),
            });
        }
    }
    out.retain(|i| i.link.starts_with("https://") || i.link.starts_with("http://"));
    Some(out)
}

/// The feed a page advertises (`<link rel="alternate" type="application/rss+xml" href=...>`),
/// resolved against `page_url`.
pub fn discover_feed(html: &str, page_url: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<link") {
        let start = from + i;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &html[start..end];
        let tl = &lower[start..end];
        if tl.contains("alternate") && (tl.contains("application/rss+xml") || tl.contains("application/atom+xml")) {
            if let Some(href) = attr_value(tag, "href") {
                return resolve(page_url, &xml::decode_entities(&href));
            }
        }
        from = end;
    }
    None
}

/// An attribute's value from one HTML tag's source (quoted or bare).
pub(crate) fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let q = rest.chars().next()?;
    if q == '"' || q == '\'' {
        rest[1..].find(q).map(|e| rest[1..1 + e].to_string())
    } else {
        Some(rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("").to_string())
    }
}

/// `href` against `base`: absolute http(s), scheme-relative or root-relative only.
fn resolve(base: &str, href: &str) -> Option<String> {
    if href.starts_with("https://") || href.starts_with("http://") {
        return Some(href.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if let Some(r) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{r}"));
    }
    if href.starts_with('/') {
        return Some(format!("{scheme}://{host}{href}"));
    }
    let dir = match rest.rfind('/') {
        Some(i) if i > host.len() => &rest[..i],
        _ => host,
    };
    Some(format!("{scheme}://{dir}/{href}"))
}

/// Query words worth matching: lower-cased, three characters or more, common words dropped.
fn terms(q: &str) -> Vec<String> {
    const STOP: [&str; 14] =
        ["the", "and", "for", "what", "with", "how", "why", "does", "are", "was", "about", "from", "this", "that"];
    let mut t: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#')
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !STOP.contains(&w.as_str()))
        .collect();
    t.sort();
    t.dedup();
    t
}

fn item_ref(i: &FeedItem, feed: &str) -> SourceRef {
    SourceRef {
        authors: i.authors.clone(),
        date: i.date.clone(),
        snippet: clip(&i.summary, SNIPPET_CHARS),
        ..SourceRef::new(ChannelId::Rss, &i.link, if i.title.is_empty() { &i.link } else { &i.title })
    }
    .with_meta("feed", feed)
}

/// Ranks items by query-term hits (title twice), newest first on ties; non-matching items drop.
pub fn rank_items(items: Vec<(FeedItem, String)>, query: &str, n: usize) -> Vec<SourceRef> {
    let t = terms(query);
    let mut scored: Vec<(usize, FeedItem, String)> = items
        .into_iter()
        .map(|(i, f)| {
            let title = i.title.to_lowercase();
            let body = i.summary.to_lowercase();
            let score = t
                .iter()
                .map(|w| 2 * usize::from(title.contains(w.as_str())) + usize::from(body.contains(w.as_str())))
                .sum();
            (score, i, f)
        })
        .filter(|(s, _, _)| t.is_empty() || *s > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.date.cmp(&a.1.date)));
    scored.into_iter().take(n).map(|(_, i, f)| item_ref(&i, &f)).collect()
}

/// The RSS/Atom channel.
pub struct RssChannel {
    spec: ChannelSpec,
    feeds: Vec<String>,
    gate: HostGate,
}

impl RssChannel {
    pub fn new(feeds: Vec<String>) -> Self {
        let feeds = feeds.into_iter().filter(|f| f.starts_with("https://")).collect();
        Self { spec: spec_of(ChannelId::Rss), feeds, gate: HostGate::new() }
    }

    pub fn feeds(&self) -> &[String] {
        &self.feeds
    }

    /// A feed's items: from the cache, else fetched politely (one autodiscovery hop for a page).
    pub fn read_feed(&self, url: &str, ctx: &ChannelCtx<'_>) -> Result<Vec<FeedItem>, ChannelError> {
        let key = CacheKey::of(&["rss-feed", url]);
        if let Some(body) = ctx.cache.and_then(|c| c.get("rss-feed", &key, FEED_TTL)) {
            if let Some(items) = parse_feed(&String::from_utf8_lossy(&body)) {
                return Ok(items);
            }
        }
        let mut current = url.to_string();
        for hop in 0..2 {
            let body = self.fetch(&current, ctx)?;
            let text = String::from_utf8_lossy(&body);
            if let Some(items) = parse_feed(&text) {
                if let Some(c) = ctx.cache {
                    let _ = c.put("rss-feed", &key, &body);
                }
                return Ok(items);
            }
            match discover_feed(&text, &current) {
                Some(next) if hop == 0 && next != current => current = next,
                _ => break,
            }
        }
        Err(ChannelError::Parse { channel: ChannelId::Rss, detail: format!("{url} is not an RSS or Atom feed") })
    }

    fn fetch(&self, url: &str, ctx: &ChannelCtx<'_>) -> Result<Vec<u8>, ChannelError> {
        let id = ChannelId::Rss;
        if !url.starts_with("https://") {
            return Err(ChannelError::Network {
                channel: id,
                detail: format!("feeds are read over https only: {url}"),
            });
        }
        self.gate.admit(url, ctx.http, ctx.cancel).map_err(|r| match r {
            Refusal::Cancelled => ChannelError::Cancelled,
            other => ChannelError::Network { channel: id, detail: other.to_string() },
        })?;
        Ok(check_status(id, get_raw(ctx, id, HttpRequest::get(url).accept(FEED_ACCEPT))?)?.bytes)
    }
}

impl Channel for RssChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let text = query.text.trim();
        let n = limit(query);
        let since = |r: &SourceRef| query.since_year.is_none_or(|y| r.year().is_none_or(|ry| ry >= y));
        if text.starts_with("https://") || text.starts_with("http://") {
            let items = self.read_feed(text, ctx)?;
            let mut out: Vec<SourceRef> = items.iter().map(|i| item_ref(i, text)).filter(since).collect();
            out.truncate(n);
            return Ok(out);
        }
        if self.feeds.is_empty() {
            return Err(ChannelError::NoBackend { channel: ChannelId::Rss, detail: "no feeds configured".into() });
        }
        let mut items = Vec::new();
        let mut failures = Vec::new();
        for f in &self.feeds {
            match self.read_feed(f, ctx) {
                Ok(v) => items.extend(v.into_iter().map(|i| (i, f.clone()))),
                Err(ChannelError::Cancelled) => return Err(ChannelError::Cancelled),
                Err(e) => failures.push(e.to_string()),
            }
        }
        if items.is_empty() && !failures.is_empty() {
            return Err(ChannelError::NoBackend { channel: ChannelId::Rss, detail: failures.join("; ") });
        }
        Ok(rank_items(items, text, n).into_iter().filter(since).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_normalise_from_rfc822_and_iso() {
        assert_eq!(feed_date("Tue, 10 Jun 2003 04:00:00 GMT").as_deref(), Some("2003-06-10"));
        assert_eq!(feed_date("2024-05-01T10:00:00Z").as_deref(), Some("2024-05-01"));
        assert_eq!(feed_date("nonsense"), None);
    }

    #[test]
    fn rss_rdf_and_atom_all_parse() {
        let rss = r#"<rss version="2.0"><channel><title>T</title><item><title>Rust 1.90 released</title>
            <link>https://blog.rust-lang.org/a</link><description>&lt;p&gt;New &amp;amp; shiny&lt;/p&gt;</description>
            <pubDate>Tue, 10 Jun 2025 04:00:00 GMT</pubDate></item><item><title>No link</title></item></channel></rss>"#;
        let items = parse_feed(rss).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!((items[0].summary.as_str(), items[0].date.as_deref()), ("New & shiny", Some("2025-06-10")));
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title type="html">A &lt;b&gt;bold&lt;/b&gt; post</title>
            <link rel="self" href="https://x.org/self"/><link rel="alternate" href="https://x.org/post"/>
            <updated>2024-01-02T00:00:00Z</updated><author><name>Sam</name></author><content type="html">Body</content></entry></feed>"#;
        let a = parse_feed(atom).unwrap();
        assert_eq!(
            (a[0].title.as_str(), a[0].link.as_str(), a[0].authors[0].as_str()),
            ("A bold post", "https://x.org/post", "Sam")
        );
        let rdf = r#"<rdf:RDF xmlns:rdf="r" xmlns="http://purl.org/rss/1.0/"><channel/><item><title>X</title><link>https://y.org/1</link><dc:date xmlns:dc="d">2023-03-04</dc:date></item></rdf:RDF>"#;
        assert_eq!(parse_feed(rdf).unwrap()[0].date.as_deref(), Some("2023-03-04"));
        assert!(parse_feed("<html><body>no</body></html>").is_none());
    }

    #[test]
    fn pages_advertise_feeds_and_items_rank_by_terms() {
        let html =
            r#"<html><head><link rel="alternate" type="application/rss+xml" title="x" href="/feed.xml"></head></html>"#;
        assert_eq!(discover_feed(html, "https://e.org/blog/post").as_deref(), Some("https://e.org/feed.xml"));
        assert_eq!(resolve("https://e.org/blog/post", "rss").as_deref(), Some("https://e.org/blog/rss"));
        let it = |t: &str, d: &str| FeedItem {
            title: t.into(),
            link: format!("https://e.org/{t}"),
            summary: String::new(),
            date: Some(d.into()),
            authors: vec![],
        };
        let items = vec![
            (it("cargo tips", "2024-01-01"), "f".into()),
            (it("rust cargo", "2023-01-01"), "f".into()),
            (it("python", "2025-01-01"), "f".into()),
        ];
        let r = rank_items(items, "rust cargo workspaces", 5);
        assert_eq!(r.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["rust cargo", "cargo tips"]);
    }
}
