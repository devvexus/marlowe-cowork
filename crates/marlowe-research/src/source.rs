//! What a channel returns: [`SourceRef`] with an optional [`Identity`], and the normalisers the
//! identity-key cascade (ADR-036 §4.2) runs on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

use marlowe_proto::ChannelId;

/// An identifier namespace. A channel is authoritative for some of these (ADR-036 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    Doi,
    Arxiv,
    Pmid,
    Pmcid,
    S2,
    OpenAlex,
    Qid,
    Repo,
    Url,
}

/// A GitHub repository pinned to a commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoAt {
    /// `owner/repo`, lower-cased.
    pub repo: String,
    /// Full or abbreviated commit sha; `None` when only the repository is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

/// Every identifier a route knows for its work. All normalised on construction through the
/// `with_*` setters; DOI is authoritative across the cascade.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    /// Version-stripped (`2401.12345v3` -> `2401.12345`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arxiv_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmcid: Option<String>,
    /// Semantic Scholar paper id or corpus id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s2: Option<String>,
    /// OpenAlex work id (`W…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openalex: Option<String>,
    /// Wikidata item (`Q…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoAt>,
    /// Canonical URL ([`canonical_url`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Identity {
    /// Sets the DOI. An arXiv DOI (`10.48550/arXiv.*`) is read as an arXiv id instead.
    pub fn with_doi(mut self, raw: &str) -> Self {
        match normalize_doi(raw) {
            Some(DoiKind::Doi(d)) => self.doi = Some(d),
            Some(DoiKind::Arxiv(a)) => self.arxiv_id = Some(a),
            None => {}
        }
        self
    }

    pub fn with_arxiv(mut self, raw: &str) -> Self {
        self.arxiv_id = normalize_arxiv(raw).or(self.arxiv_id);
        self
    }

    pub fn with_pmid(mut self, raw: &str) -> Self {
        let d: String = raw.trim().chars().filter(char::is_ascii_digit).collect();
        if !d.is_empty() {
            self.pmid = Some(d);
        }
        self
    }

    pub fn with_pmcid(mut self, raw: &str) -> Self {
        let t = raw.trim().to_ascii_uppercase();
        let digits = t.strip_prefix("PMC").unwrap_or(&t);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            self.pmcid = Some(format!("PMC{digits}"));
        }
        self
    }

    pub fn with_s2(mut self, raw: &str) -> Self {
        let t = raw.trim();
        if !t.is_empty() {
            self.s2 = Some(t.to_ascii_lowercase());
        }
        self
    }

    pub fn with_openalex(mut self, raw: &str) -> Self {
        let t = raw.trim().trim_start_matches("https://openalex.org/").to_ascii_uppercase();
        if t.starts_with('W') && t.len() > 1 && t[1..].chars().all(|c| c.is_ascii_digit()) {
            self.openalex = Some(t);
        }
        self
    }

    pub fn with_qid(mut self, raw: &str) -> Self {
        let t = raw.trim().rsplit('/').next().unwrap_or("").to_ascii_uppercase();
        if t.starts_with('Q') && t.len() > 1 && t[1..].chars().all(|c| c.is_ascii_digit()) {
            self.qid = Some(t);
        }
        self
    }

    pub fn with_repo(mut self, owner_repo: &str, sha: Option<&str>) -> Self {
        let r = owner_repo.trim().trim_matches('/').to_ascii_lowercase();
        if r.split('/').count() == 2 && !r.contains("//") {
            let sha = sha.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
            self.repo = Some(RepoAt { repo: r, sha });
        }
        self
    }

    pub fn with_url(mut self, raw: &str) -> Self {
        self.url = canonical_url(raw).or(self.url);
        self
    }

    /// The identifiers set, as `(namespace, value)`, in cascade order (DOI first).
    pub fn keys(&self) -> Vec<(Namespace, String)> {
        let mut out = Vec::new();
        let mut push = |ns, v: &Option<String>| {
            if let Some(v) = v {
                out.push((ns, v.clone()));
            }
        };
        push(Namespace::Doi, &self.doi);
        push(Namespace::Arxiv, &self.arxiv_id);
        push(Namespace::Pmid, &self.pmid);
        push(Namespace::Pmcid, &self.pmcid);
        push(Namespace::S2, &self.s2);
        push(Namespace::OpenAlex, &self.openalex);
        push(Namespace::Qid, &self.qid);
        if let Some(r) = &self.repo {
            out.push((Namespace::Repo, r.sha.as_ref().map_or(r.repo.clone(), |s| format!("{}@{s}", r.repo))));
        }
        if let Some(u) = &self.url {
            out.push((Namespace::Url, u.clone()));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.keys().is_empty()
    }

    /// Fills unset fields from `other`; set fields are kept (precedence is the caller's order).
    pub fn absorb(&mut self, other: &Identity) {
        macro_rules! fill {
            ($($f:ident),*) => {$( if self.$f.is_none() { self.$f = other.$f.clone(); } )*};
        }
        fill!(doi, arxiv_id, pmid, pmcid, s2, openalex, qid, repo, url);
    }
}

/// Which trust bucket a source belongs to. Memory is never merged with fetched content
/// (ADR-037 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustBucket {
    /// Recalled from the user's own memory.
    Memory,
    /// Anything a channel returned or a fetch read.
    Untrusted,
}

/// One channel's record of one work: a route (ADR-036 §4.1). Kept verbatim; dedup groups routes
/// into sources and never rewrites them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub channel: ChannelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// As the channel gave it, ISO-8601 or a bare year.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    /// The channel reports this work retracted. Any route saying so wins (ADR-036 §4.4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retracted: bool,
    /// Channel-specific signals for selection, verbatim from structured fields: `venue`,
    /// `cited_by`, `stars`, `points`, `score`, `pdf`, `linked_url`, `backend`, ... Untrusted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

impl SourceRef {
    pub fn new(channel: ChannelId, url: &str, title: &str) -> Self {
        Self {
            channel,
            identity: None,
            url: url.to_string(),
            title: title.to_string(),
            authors: Vec::new(),
            date: None,
            snippet: String::new(),
            retracted: false,
            meta: BTreeMap::new(),
        }
    }

    /// Sets a `meta` entry when `value` is non-empty.
    pub fn with_meta(mut self, key: &str, value: impl Into<String>) -> Self {
        let v = value.into();
        if !v.trim().is_empty() {
            self.meta.insert(key.to_string(), v);
        }
        self
    }

    pub fn bucket(&self) -> TrustBucket {
        if self.channel == ChannelId::Memory {
            TrustBucket::Memory
        } else {
            TrustBucket::Untrusted
        }
    }

    /// The first four-digit year in `date`.
    pub fn year(&self) -> Option<u16> {
        let d = self.date.as_deref()?;
        let b = d.as_bytes();
        (0..b.len().saturating_sub(3))
            .find(|&i| b[i..i + 4].iter().all(u8::is_ascii_digit))
            .and_then(|i| d[i..i + 4].parse().ok())
    }

    /// The route's identity, with the canonical URL filled in from `url` when unset.
    pub fn effective_identity(&self) -> Identity {
        let mut id = self.identity.clone().unwrap_or_default();
        if id.url.is_none() {
            id.url = canonical_url(&self.url);
        }
        id
    }
}

/// A DOI string, or an arXiv DOI read as an arXiv id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoiKind {
    Doi(String),
    Arxiv(String),
}

/// Strips `https://doi.org/`, `dx.doi.org`, `doi:`; casefolds; trims trailing punctuation.
/// `10.48550/arXiv.*` is arXiv's own DOI and comes back as [`DoiKind::Arxiv`].
pub fn normalize_doi(raw: &str) -> Option<DoiKind> {
    let mut t = raw.trim().to_lowercase();
    for p in ["https://doi.org/", "http://doi.org/", "https://dx.doi.org/", "http://dx.doi.org/", "doi.org/", "doi:"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest.trim().to_string();
            break;
        }
    }
    let t = t.trim_end_matches(|c: char| c.is_whitespace() || ".,;:)]}>\"'".contains(c));
    if !t.starts_with("10.") || !t.contains('/') {
        return None;
    }
    if let Some(a) = t.strip_prefix("10.48550/arxiv.") {
        return normalize_arxiv(a).map(DoiKind::Arxiv);
    }
    Some(DoiKind::Doi(t.to_string()))
}

/// `arXiv:2401.12345v3`, `https://arxiv.org/abs/2401.12345v3`, `hep-th/9901001v2` ->
/// version-stripped id. `None` when it is not an arXiv id.
pub fn normalize_arxiv(raw: &str) -> Option<String> {
    let mut t = raw.trim().to_lowercase();
    for p in ["https://", "http://", "www.", "export.", "arxiv.org/", "abs/", "pdf/", "html/", "arxiv:"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest.to_string();
        }
    }
    let t = t.trim_end_matches(".pdf").trim_end_matches('/');
    let base = match t.rfind('v') {
        Some(i) if i > 0 && t[i + 1..].chars().all(|c| c.is_ascii_digit()) && !t[i + 1..].is_empty() => &t[..i],
        _ => t,
    };
    let new_style = base.split_once('.').is_some_and(|(a, b)| {
        a.len() == 4
            && a.chars().all(|c| c.is_ascii_digit())
            && (4..=5).contains(&b.len())
            && b.chars().all(|c| c.is_ascii_digit())
    });
    let old_style = base.split_once('/').is_some_and(|(a, b)| {
        !a.is_empty()
            && a.chars().all(|c| c.is_ascii_lowercase() || c == '-' || c == '.')
            && b.len() == 7
            && b.chars().all(|c| c.is_ascii_digit())
    });
    (new_style || old_style).then(|| base.to_string())
}

/// Tracking parameters dropped from canonical URLs.
const TRACKING: [&str; 6] = ["fbclid", "gclid", "ref", "source", "mc_cid", "mc_eid"];

/// ADR-036 §4.2 tier 3: https; host casefolded, `www.` dropped; fragment and tracking parameters
/// (`utm_*`, `fbclid`, …) removed; remaining query sorted; trailing `/` and `/index.html`
/// stripped. Meaningful parameters (`?id=447`) are kept. `None` for a non-http(s) URL.
pub fn canonical_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    let rest = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
        .or_else(|| t.get(..8).filter(|p| p.eq_ignore_ascii_case("https://")).map(|_| &t[8..]))
        .or_else(|| t.get(..7).filter(|p| p.eq_ignore_ascii_case("http://")).map(|_| &t[7..]))?;
    let rest = rest.split('#').next().unwrap_or("");
    let (hostpath, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host, path) = match hostpath.find('/') {
        Some(i) => (&hostpath[..i], &hostpath[i..]),
        None => (hostpath, ""),
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let host = host.strip_suffix(":443").unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    // arXiv's abs, pdf and html pages of one id (any version) are one record on the authority's
    // own host: found live, a paper and its `html/<id>v1` page counted as two independent roots.
    if host == "arxiv.org" || host == "export.arxiv.org" {
        let id = ["/abs/", "/pdf/", "/html/"].iter().find_map(|p| path.strip_prefix(p)).and_then(|r| {
            let r = r.trim_end_matches('/');
            normalize_arxiv(r.strip_suffix(".pdf").unwrap_or(r))
        });
        if let Some(id) = id {
            return Some(format!("https://arxiv.org/abs/{id}"));
        }
    }
    if let Some(landing) = scholarly_landing(host, path, query) {
        return Some(landing);
    }
    let mut path = path.to_string();
    for suffix in ["/index.html", "/index.htm"] {
        if path.to_ascii_lowercase().ends_with(suffix) {
            path.truncate(path.len() - suffix.len());
        }
    }
    while path.ends_with('/') {
        path.pop();
    }
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("").to_ascii_lowercase();
            !k.starts_with("utm_") && !TRACKING.contains(&k.as_str())
        })
        .collect();
    params.sort_unstable();
    let q = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
    Some(format!("https://{host}{path}{q}"))
}

/// One paper's landing page and its PDF on the proceedings hosts, as one URL. Found live: one
/// work fetched as four routes (abs, versioned abs, pdf, html) spent four sources of the budget
/// and counted as independent roots.
fn scholarly_landing(host: &str, path: &str, query: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    match host {
        // `/2026.acl-long.276/` and `/2026.acl-long.276.pdf`.
        "aclanthology.org" => {
            let id = trimmed.strip_prefix('/')?;
            let id = id.strip_suffix(".pdf").unwrap_or(id);
            (!id.is_empty() && !id.contains('/') && id.contains('.')).then(|| format!("https://aclanthology.org/{id}"))
        }
        // `/forum?id=X` and `/pdf?id=X` (a `noteId` names a comment on the same paper).
        "openreview.net" if matches!(trimmed, "/forum" | "/pdf") => {
            let id = query.split('&').find_map(|p| p.strip_prefix("id="))?;
            (!id.is_empty()).then(|| format!("https://openreview.net/forum?id={id}"))
        }
        // `/v202/name23a.html` and `/v202/name23a/name23a.pdf`.
        "proceedings.mlr.press" => {
            let mut parts = trimmed.trim_start_matches('/').split('/');
            let volume = parts.next().filter(|v| v.starts_with('v'))?;
            let name = parts.next()?;
            let name = name.strip_suffix(".html").or_else(|| name.strip_suffix(".pdf")).unwrap_or(name);
            Some(format!("https://proceedings.mlr.press/{volume}/{name}"))
        }
        // `.../hash/<32 hex>-Abstract-Conference.html` and `.../file/<32 hex>-Paper-Conference.pdf`.
        "proceedings.neurips.cc" | "papers.nips.cc" | "papers.neurips.cc" => {
            let last = trimmed.rsplit('/').next()?;
            let hash = last.split('-').next()?;
            (hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| format!("https://proceedings.neurips.cc/paper/{}", hash.to_ascii_lowercase()))
        }
        _ => None,
    }
}

/// ADR-036 §4.2 tier 2: NFKC, casefold, diacritics stripped, non-alphanumerics removed,
/// whitespace collapsed. Subtitles are kept; no stemming.
pub fn normalize_title(raw: &str) -> String {
    let folded: String =
        raw.nfkd().filter(|c| !is_combining_mark(*c)).collect::<String>().nfkc().collect::<String>().to_lowercase();
    let spaced: String = folded.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first author's surname, normalised like a title: `"Hu, Edward J."` and `"Edward J. Hu"`
/// both give `"hu"`.
pub fn first_author_surname(authors: &[String]) -> Option<String> {
    let a = authors.first()?.trim();
    let surname = match a.split_once(',') {
        Some((last, _)) => last,
        None => a.split_whitespace().last().unwrap_or(""),
    };
    let n = normalize_title(surname);
    (!n.is_empty()).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dois_normalise_and_arxiv_dois_become_arxiv_ids() {
        assert_eq!(normalize_doi("https://doi.org/10.1000/ABC.def."), Some(DoiKind::Doi("10.1000/abc.def".into())));
        assert_eq!(
            normalize_doi("doi: 10.1145/3292500.3330701)"),
            Some(DoiKind::Doi("10.1145/3292500.3330701".into()))
        );
        assert_eq!(normalize_doi("10.48550/arXiv.2106.09685"), Some(DoiKind::Arxiv("2106.09685".into())));
        assert_eq!(normalize_doi("not a doi"), None);
        let id = Identity::default().with_doi("https://doi.org/10.48550/arXiv.2106.09685");
        assert_eq!((id.doi, id.arxiv_id.as_deref()), (None, Some("2106.09685")));
    }

    #[test]
    fn arxiv_ids_strip_versions_and_urls() {
        for raw in [
            "2106.09685",
            "arXiv:2106.09685v2",
            "https://arxiv.org/abs/2106.09685v3",
            "https://arxiv.org/pdf/2106.09685v1.pdf",
            "arxiv.org/html/2106.09685v2",
        ] {
            assert_eq!(normalize_arxiv(raw).as_deref(), Some("2106.09685"), "{raw}");
        }
        assert_eq!(normalize_arxiv("hep-th/9901001v2").as_deref(), Some("hep-th/9901001"));
        assert_eq!(normalize_arxiv("hello"), None);
    }

    #[test]
    fn canonical_urls_drop_tracking_and_keep_meaningful_params() {
        assert_eq!(
            canonical_url("http://WWW.Example.com/a/b/?utm_source=x&id=447&fbclid=y#top").as_deref(),
            Some("https://example.com/a/b?id=447")
        );
        assert_eq!(canonical_url("https://example.com/docs/index.html").as_deref(), Some("https://example.com/docs"));
        assert_eq!(canonical_url("https://example.com/?b=2&a=1").as_deref(), Some("https://example.com?a=1&b=2"));
        assert_ne!(canonical_url("https://x.org/?id=1"), canonical_url("https://x.org/?id=2"));
        assert_eq!(canonical_url("ftp://x.org/a"), None);
        for u in [
            "https://arxiv.org/abs/2601.13580",
            "https://arxiv.org/html/2601.13580v1",
            "https://arxiv.org/html/2601.13580v1/#S2",
            "http://export.arxiv.org/pdf/2601.13580v2.pdf",
            "https://www.arxiv.org/abs/2601.13580v3?context=cs",
        ] {
            assert_eq!(canonical_url(u).as_deref(), Some("https://arxiv.org/abs/2601.13580"), "{u}");
        }
        assert_eq!(canonical_url("https://arxiv.org/list/cs.CL/recent").as_deref(), Some("https://arxiv.org/list/cs.CL/recent"));
    }

    #[test]
    fn proceedings_landing_pages_and_their_pdfs_are_one_url() {
        let same = |a: &str, b: &str| {
            assert_eq!(canonical_url(a), canonical_url(b), "{a} vs {b}");
            assert!(canonical_url(a).is_some());
        };
        same("https://aclanthology.org/2026.acl-long.276/", "https://aclanthology.org/2026.acl-long.276.pdf");
        same("https://openreview.net/forum?id=AbC123", "https://openreview.net/pdf?id=AbC123");
        same("https://openreview.net/forum?id=AbC123&noteId=Zz9", "https://openreview.net/forum?id=AbC123");
        same("https://proceedings.mlr.press/v202/frantar23a.html", "https://proceedings.mlr.press/v202/frantar23a/frantar23a.pdf");
        same(
            "https://proceedings.neurips.cc/paper_files/paper/2023/hash/0123456789abcdef0123456789abcdef-Abstract-Conference.html",
            "https://papers.nips.cc/paper_files/paper/2023/file/0123456789ABCDEF0123456789abcdef-Paper-Conference.pdf",
        );
        // Different papers stay apart, and other pages of these hosts are left alone.
        assert_ne!(canonical_url("https://aclanthology.org/2026.acl-long.276"), canonical_url("https://aclanthology.org/2026.acl-long.277"));
        assert_ne!(canonical_url("https://openreview.net/forum?id=A"), canonical_url("https://openreview.net/forum?id=B"));
        assert_eq!(canonical_url("https://aclanthology.org/events/acl-2026/").as_deref(), Some("https://aclanthology.org/events/acl-2026"));
        assert_eq!(canonical_url("https://openreview.net/group?id=ICLR.cc").as_deref(), Some("https://openreview.net/group?id=ICLR.cc"));
    }

    #[test]
    fn titles_and_surnames_normalise_without_fuzz() {
        assert_eq!(
            normalize_title("LoRA: Low-Rank Adaptation of Large Language Models"),
            "lora low rank adaptation of large language models"
        );
        assert_eq!(normalize_title("Ｓｃｈｒöｄｉｎｇｅｒ’s  Cat"), "schrodinger s cat");
        assert_eq!(first_author_surname(&["Hu, Edward J.".into()]).as_deref(), Some("hu"));
        assert_eq!(first_author_surname(&["Edward J. Hu".into(), "Y. Shen".into()]).as_deref(), Some("hu"));
    }

    #[test]
    fn identities_list_keys_in_cascade_order_and_absorb() {
        let id = Identity::default()
            .with_url("https://arxiv.org/abs/2106.09685")
            .with_arxiv("2106.09685v2")
            .with_doi("10.1000/X");
        let ns: Vec<Namespace> = id.keys().into_iter().map(|(n, _)| n).collect();
        assert_eq!(ns, [Namespace::Doi, Namespace::Arxiv, Namespace::Url]);
        let mut a = Identity::default().with_qid("http://www.wikidata.org/entity/Q42");
        a.absorb(&Identity::default().with_qid("Q1").with_pmcid("pmc123").with_repo("Owner/Repo", Some("ABC")));
        assert_eq!(a.qid.as_deref(), Some("Q42"));
        assert_eq!(a.pmcid.as_deref(), Some("PMC123"));
        assert_eq!(
            a.keys().iter().find(|(n, _)| *n == Namespace::Repo).map(|(_, v)| v.as_str()),
            Some("owner/repo@abc")
        );
        let s = SourceRef {
            date: Some("2021-06-17".into()),
            ..SourceRef::new(ChannelId::Arxiv, "https://arxiv.org/abs/2106.09685", "LoRA")
        };
        assert_eq!(s.year(), Some(2021));
        assert_eq!(s.bucket(), TrustBucket::Untrusted);
        assert_eq!(SourceRef::new(ChannelId::Memory, "mem:1", "m").bucket(), TrustBucket::Memory);
    }
}
