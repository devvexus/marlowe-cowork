//! Engineering and discussion channels: GitHub REST (unauthenticated repository and issue
//! search), Hacker News through Algolia, and the Stack Exchange API. Discussion is dated and
//! derivative: an HN thread is its own artifact that points at what it discusses (`linked_url`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use marlowe_proto::ChannelId;
use serde_json::Value;

use super::SNIPPET_CHARS;
use super::{check_status, clip, enc, get_json, get_raw, iso_date, limit, num, parse_json, s, spec_of, year_start};
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpRequest};
use crate::source::{Identity, Namespace, SourceRef};
use crate::xml::{decode_entities, strip_tags};

// ------------------------------------------------------------------ GitHub

const GITHUB_ACCEPT: &str = "application/vnd.github+json";

pub struct GithubChannel {
    spec: ChannelSpec,
}

impl Default for GithubChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Is this query for issues and pull requests rather than repositories (GitHub's own `is:`
/// qualifiers)?
fn wants_issues(text: &str) -> bool {
    text.split_whitespace().any(|w| matches!(w, "is:issue" | "is:pr" | "type:issue" | "type:pr"))
}

/// One repository from `/search/repositories` or `/repos/{owner}/{repo}`.
pub fn github_repo(r: &Value) -> Option<SourceRef> {
    let full = s(r, "full_name")?;
    let url = s(r, "html_url").unwrap_or_else(|| format!("https://github.com/{full}"));
    let owner = r.get("owner").and_then(|o| s(o, "login")).into_iter().collect();
    let topics: Vec<String> = r
        .get("topics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(String::from))
        .collect();
    let mut snippet = s(r, "description").unwrap_or_default();
    if !topics.is_empty() {
        snippet.push_str(&format!(" [topics: {}]", topics.join(", ")));
    }
    let flag = |k: &str| if r.get(k).and_then(Value::as_bool) == Some(true) { "true" } else { "" };
    Some(
        SourceRef {
            identity: Some(Identity::default().with_repo(&full, None)),
            authors: owner,
            date: s(r, "pushed_at").or_else(|| s(r, "updated_at")),
            snippet: clip(snippet.trim(), SNIPPET_CHARS),
            ..SourceRef::new(ChannelId::Github, &url, &full)
        }
        .with_meta("date_kind", "last_push")
        .with_meta("stars", num(r, "stargazers_count").unwrap_or_default())
        .with_meta("language", s(r, "language").unwrap_or_default())
        .with_meta("license", r.get("license").and_then(|l| s(l, "spdx_id")).unwrap_or_default())
        .with_meta("created", s(r, "created_at").unwrap_or_default())
        .with_meta("archived", flag("archived"))
        .with_meta("fork", flag("fork")),
    )
}

/// One issue or pull request from `/search/issues`.
pub fn github_issue(i: &Value) -> Option<SourceRef> {
    let url = s(i, "html_url")?;
    let title = s(i, "title")?;
    let repo =
        s(i, "repository_url").and_then(|u| u.split_once("/repos/").map(|(_, r)| r.to_string())).unwrap_or_default();
    let kind = if i.get("pull_request").is_some() { "pull_request" } else { "issue" };
    Some(
        SourceRef {
            authors: i.get("user").and_then(|u| s(u, "login")).into_iter().collect(),
            date: s(i, "created_at"),
            snippet: clip(
                &s(i, "body").unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" "),
                SNIPPET_CHARS,
            ),
            ..SourceRef::new(ChannelId::Github, &url, &title)
        }
        .with_meta("repo", repo)
        .with_meta("kind", kind)
        .with_meta("state", s(i, "state").unwrap_or_default())
        .with_meta("comments", num(i, "comments").unwrap_or_default()),
    )
}

impl GithubChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::Github) }
    }

    pub fn search_url(&self, q: &ChannelQuery) -> String {
        let issues = wants_issues(&q.text);
        let mut text = q.text.trim().to_string();
        if let Some(y) = q.since_year {
            text.push_str(&format!(" {}:>={y}-01-01", if issues { "created" } else { "pushed" }));
        }
        let path = if issues { "issues" } else { "repositories" };
        format!("{}/search/{path}?q={}&per_page={}", self.spec.endpoint, enc(&text), limit(q).min(100))
    }

    fn json(&self, url: String, ctx: &ChannelCtx<'_>) -> Result<Value, ChannelError> {
        let id = ChannelId::Github;
        let r = get_raw(ctx, id, HttpRequest::get(url).accept(GITHUB_ACCEPT))?;
        // GitHub answers an exhausted unauthenticated quota with 403 and a message, not 429.
        if r.status == 403 && String::from_utf8_lossy(&r.bytes).to_ascii_lowercase().contains("rate limit") {
            return Err(ChannelError::RateLimited {
                channel: id,
                retry_after: r.retry_after.or(Some(Duration::from_secs(60))),
            });
        }
        parse_json(id, &check_status(id, r)?.bytes)
    }
}

impl Channel for GithubChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let v = self.json(self.search_url(query), ctx)?;
        let items = v.get("items").and_then(Value::as_array).ok_or_else(|| ChannelError::Parse {
            channel: ChannelId::Github,
            detail: s(&v, "message").unwrap_or_else(|| "no `items` array".into()),
        })?;
        let parse = if wants_issues(&query.text) { github_issue } else { github_repo };
        Ok(items.iter().filter_map(parse).collect())
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let repo = (ns == Namespace::Repo)
            .then(|| Identity::default().with_repo(id.split('@').next().unwrap_or(id), None).repo);
        let Some(Some(repo)) = repo else {
            return Err(ChannelError::NoLookup { channel: ChannelId::Github, namespace: ns });
        };
        match self.json(format!("{}/repos/{}", self.spec.endpoint, repo.repo), ctx) {
            Ok(v) => Ok(github_repo(&v)),
            Err(ChannelError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ------------------------------------------------------------------ Hacker News

pub struct HackerNewsChannel {
    spec: ChannelSpec,
}

impl Default for HackerNewsChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// One Algolia hit: the HN thread is the artifact; the story's own URL is `linked_url`.
pub fn hn_hit(h: &Value) -> Option<SourceRef> {
    let id = s(h, "objectID")?;
    let title = s(h, "title").or_else(|| s(h, "story_title"))?;
    let url = format!("https://news.ycombinator.com/item?id={id}");
    let text = s(h, "story_text").or_else(|| s(h, "comment_text")).map(|t| strip_tags(&t)).unwrap_or_default();
    Some(
        SourceRef {
            authors: s(h, "author").into_iter().collect(),
            date: s(h, "created_at"),
            snippet: clip(&text, SNIPPET_CHARS),
            ..SourceRef::new(ChannelId::HackerNews, &url, &title)
        }
        .with_meta("points", num(h, "points").unwrap_or_default())
        .with_meta("comments", num(h, "num_comments").unwrap_or_default())
        .with_meta("linked_url", s(h, "url").unwrap_or_default()),
    )
}

impl HackerNewsChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::HackerNews) }
    }

    pub fn search_url(&self, q: &ChannelQuery) -> String {
        let mut u = format!(
            "{}/search?query={}&tags=story&hitsPerPage={}",
            self.spec.endpoint,
            enc(q.text.trim()),
            limit(q).min(50)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&numericFilters={}", enc(&format!("created_at_i>{}", year_start(y)))));
        }
        u
    }
}

impl Channel for HackerNewsChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let v = get_json(ctx, ChannelId::HackerNews, self.search_url(query))?;
        let hits = v
            .get("hits")
            .and_then(Value::as_array)
            .ok_or_else(|| ChannelError::Parse { channel: ChannelId::HackerNews, detail: "no `hits` array".into() })?;
        Ok(hits.iter().filter_map(hn_hit).collect())
    }
}

// ------------------------------------------------------------------ Stack Exchange

/// Stack Exchange's keyless API. `site:<name>` in the query picks the site; the API's `backoff`
/// field is honoured by holding every later request on this channel until it passes.
pub struct StackExchangeChannel {
    spec: ChannelSpec,
    site: String,
    not_before: Mutex<Option<Instant>>,
}

fn site_name(raw: &str) -> Option<String> {
    let t = raw.trim().to_ascii_lowercase();
    (!t.is_empty() && t.len() <= 40 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
        .then_some(t)
}

/// One question from `/search/advanced` with `filter=withbody`.
pub fn se_question(q: &Value, site: &str) -> Option<SourceRef> {
    let link = s(q, "link")?;
    let title = decode_entities(&s(q, "title")?);
    let date = q.get("creation_date").and_then(Value::as_i64).map(iso_date);
    let tags: Vec<String> = q
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(String::from))
        .collect();
    let flag = |k: &str| if q.get(k).and_then(Value::as_bool) == Some(true) { "true" } else { "false" };
    Some(
        SourceRef {
            authors: q
                .get("owner")
                .and_then(|o| s(o, "display_name"))
                .map(|n| decode_entities(&n))
                .into_iter()
                .collect(),
            date,
            snippet: clip(&strip_tags(&s(q, "body").unwrap_or_default()), SNIPPET_CHARS),
            ..SourceRef::new(ChannelId::StackExchange, &link, &title)
        }
        .with_meta("site", site)
        .with_meta("score", num(q, "score").unwrap_or_default())
        .with_meta("answers", num(q, "answer_count").unwrap_or_default())
        .with_meta("answered", flag("is_answered"))
        .with_meta("accepted_answer", num(q, "accepted_answer_id").unwrap_or_default())
        .with_meta("tags", tags.join(" ")),
    )
}

impl StackExchangeChannel {
    pub fn new(site: &str) -> Self {
        Self {
            spec: spec_of(ChannelId::StackExchange),
            site: site_name(site).unwrap_or_else(|| "stackoverflow".into()),
            not_before: Mutex::new(None),
        }
    }

    /// The site and the query text with any `site:` token removed.
    fn split_site(&self, text: &str) -> (String, String) {
        let mut site = self.site.clone();
        let mut words = Vec::new();
        for w in text.split_whitespace() {
            match w.strip_prefix("site:").and_then(site_name) {
                Some(s) => site = s,
                None => words.push(w),
            }
        }
        (site, words.join(" "))
    }

    pub fn search_url(&self, q: &ChannelQuery) -> (String, String) {
        let (site, text) = self.split_site(&q.text);
        let mut u = format!(
            "{}/search/advanced?order=desc&sort=relevance&q={}&site={}&pagesize={}&filter=withbody",
            self.spec.endpoint,
            enc(&text),
            enc(&site),
            limit(q).min(100)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&fromdate={}", year_start(y)));
        }
        (u, site)
    }

    pub fn similar_url(&self, q: &ChannelQuery) -> String {
        let (site, text) = self.split_site(&q.text);
        let mut u = format!(
            "{}/similar?order=desc&sort=relevance&title={}&site={}&pagesize={}&filter=withbody",
            self.spec.endpoint,
            enc(&text),
            enc(&site),
            limit(q).min(100)
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&fromdate={}", year_start(y)));
        }
        u
    }

    /// One API call: `items`, honouring `backoff`; throttling is a rate limit, not a failure.
    fn items(&self, url: String, ctx: &ChannelCtx<'_>) -> Result<Vec<Value>, ChannelError> {
        self.hold(ctx)?;
        let id = ChannelId::StackExchange;
        let r = get_raw(ctx, id, HttpRequest::get(url).accept("application/json"))?;
        let parsed = parse_json(id, &r.bytes);
        if let Some(name) = parsed.as_ref().ok().and_then(|v| s(v, "error_name")) {
            if name.contains("throttle") || r.status == 429 {
                return Err(ChannelError::RateLimited { channel: id, retry_after: Some(Duration::from_secs(60)) });
            }
            let msg = parsed.as_ref().ok().and_then(|v| s(v, "error_message")).unwrap_or_default();
            return Err(ChannelError::Parse { channel: id, detail: format!("{name}: {msg}") });
        }
        check_status(id, r)?;
        let v = parsed?;
        if let Some(b) = v.get("backoff").and_then(Value::as_u64) {
            *self.not_before.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now() + Duration::from_secs(b));
        }
        Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn hold(&self, ctx: &ChannelCtx<'_>) -> Result<(), ChannelError> {
        let until = *self.not_before.lock().unwrap_or_else(|p| p.into_inner());
        while let Some(u) = until.filter(|u| *u > Instant::now()) {
            if ctx.cancel.is_cancelled() {
                return Err(ChannelError::Cancelled);
            }
            std::thread::sleep((u - Instant::now()).min(Duration::from_millis(100)));
        }
        Ok(())
    }
}

impl Channel for StackExchangeChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    /// Full-text search first; when it finds nothing (it ANDs every word), `/similar` matches
    /// the text as a question title, which is what a research sub-question usually is.
    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let (url, site) = self.search_url(query);
        let mut items = self.items(url, ctx)?;
        if items.is_empty() {
            items = self.items(self.similar_url(query), ctx)?;
        }
        Ok(items.iter().filter_map(|q| se_question(q, &site)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_pick_endpoints_sites_and_dates() {
        let g = GithubChannel::new();
        assert!(g
            .search_url(&ChannelQuery::new("tokio runtime", 5))
            .contains("/search/repositories?q=tokio%20runtime&per_page=5"));
        let q = ChannelQuery { since_year: Some(2024), ..ChannelQuery::new("is:issue ort cuda", 5) };
        assert!(g.search_url(&q).contains("/search/issues?q=is%3Aissue%20ort%20cuda%20created%3A%3E%3D2024-01-01"));
        let se = StackExchangeChannel::new("stackoverflow");
        let (u, site) = se.search_url(&ChannelQuery::new("site:superuser wsl dns", 3));
        assert_eq!(site, "superuser");
        assert!(u.contains("q=wsl%20dns&site=superuser&pagesize=3"), "{u}");
        assert_eq!(StackExchangeChannel::new("evil/site").site, "stackoverflow");
        let hn = HackerNewsChannel::new();
        let q = ChannelQuery { since_year: Some(2024), ..ChannelQuery::new("rust", 2) };
        assert!(hn.search_url(&q).ends_with("numericFilters=created_at_i%3E1704067200"));
    }
}
