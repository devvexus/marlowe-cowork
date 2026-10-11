//! Politeness for page fetches: robots.txt (RFC 9309) and a per-host rate limit, honouring
//! `Crawl-delay`. Shared by the corpus fetcher; no evasion of any kind lives here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_model::CancelToken;

use crate::channel::{HttpGet, HttpRequest, Limiter, RateLimit, USER_AGENT};

/// The product token matched against `User-agent:` lines.
pub const ROBOTS_TOKEN: &str = "marlowecowork";
/// Default spacing between requests to one host.
pub const DEFAULT_HOST_GAP: Duration = Duration::from_secs(1);
/// `Crawl-delay` above this is treated as "do not fetch from this host in a research run".
pub const MAX_CRAWL_DELAY: Duration = Duration::from_secs(30);
/// robots.txt bodies above this are read only this far (RFC 9309 §2.5: at least 500 KiB).
pub const MAX_ROBOTS_BYTES: usize = 512 * 1024;
/// A fetched robots.txt is used this long (RFC 9309 §2.4: at most 24 hours).
pub const ROBOTS_TTL: Duration = Duration::from_secs(24 * 3600);
/// An unreachable or 5xx robots.txt denies everything, and is asked again after this.
pub const ROBOTS_RETRY: Duration = Duration::from_secs(5 * 60);
/// Hosts remembered at once; the least recently used is dropped beyond this.
pub const MAX_HOSTS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    path: String,
}

/// A parsed robots.txt, reduced to the group that applies to us.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Robots {
    rules: Vec<Rule>,
    pub crawl_delay: Option<Duration>,
    /// Everything disallowed (an unreachable robots.txt, RFC 9309 §2.3.1.4).
    deny_all: bool,
}

impl Robots {
    pub fn allow_all() -> Self {
        Self::default()
    }

    pub fn deny_all() -> Self {
        Self { deny_all: true, ..Self::default() }
    }

    /// Parses `body`, keeping the group for [`ROBOTS_TOKEN`] if there is one, else `*`.
    pub fn parse(body: &str) -> Self {
        // (agents, rules, delay) per group; consecutive User-agent lines share a group.
        let mut groups: Vec<(Vec<String>, Vec<Rule>, Option<Duration>)> = Vec::new();
        let mut in_agents = false;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "user-agent" => {
                    if !in_agents {
                        groups.push((Vec::new(), Vec::new(), None));
                    }
                    in_agents = true;
                    if let Some(g) = groups.last_mut() {
                        g.0.push(v.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    in_agents = false;
                    if let Some(g) = groups.last_mut() {
                        if !v.is_empty() {
                            g.1.push(Rule { allow: k == "allow", path: v.to_string() });
                        }
                    }
                }
                "crawl-delay" => {
                    in_agents = false;
                    if let (Some(g), Ok(s)) = (groups.last_mut(), v.parse::<f64>()) {
                        if s.is_finite() && s >= 0.0 {
                            g.2 = Some(Duration::from_secs_f64(s.min(86_400.0)));
                        }
                    }
                }
                _ => in_agents = false,
            }
        }
        let pick = |want: &dyn Fn(&str) -> bool| {
            let mut rules = Vec::new();
            let mut delay = None;
            let mut found = false;
            for (agents, r, d) in &groups {
                if agents.iter().any(|a| want(a)) {
                    found = true;
                    rules.extend(r.iter().cloned());
                    delay = delay.or(*d);
                }
            }
            found.then_some((rules, delay))
        };
        let (rules, crawl_delay) = pick(&|a| ROBOTS_TOKEN.starts_with(a) && !a.is_empty() && a != "*")
            .or_else(|| pick(&|a| a == "*"))
            .unwrap_or_default();
        Self { rules, crawl_delay, deny_all: false }
    }

    /// Whether `path` (with query) may be fetched: the longest matching rule wins, allow on a tie.
    pub fn allowed(&self, path: &str) -> bool {
        if self.deny_all {
            return false;
        }
        let path = if path.is_empty() { "/" } else { path };
        let mut best: Option<(usize, bool)> = None;
        for r in &self.rules {
            if pattern_matches(&r.path, path) {
                let len = r.path.len();
                best = match best {
                    Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
                    _ => Some((len, r.allow)),
                };
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// RFC 9309 path matching: prefix match with `*` wildcards and a `$` end anchor.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut at = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return false;
            }
            at = part.len();
        } else if i == parts.len() - 1 && anchored {
            return path.len() >= at + part.len() && path.ends_with(part);
        } else {
            match path[at..].find(part) {
                Some(j) => at += j + part.len(),
                None => return false,
            }
        }
    }
    !anchored || at == path.len()
}

/// Why a page may not be fetched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("robots.txt on {host} disallows {path}")]
    Robots { host: String, path: String },
    #[error("{host} asks for a crawl delay of {delay:?}, above the {MAX_CRAWL_DELAY:?} research ceiling")]
    CrawlDelay { host: String, delay: Duration },
    #[error("not an http(s) URL: {0}")]
    NotHttp(String),
    #[error("cancelled")]
    Cancelled,
}

/// One origin's robots, its limiter, when the robots expire and when the host was last used.
struct HostEntry {
    robots: Arc<Robots>,
    limiter: Arc<Limiter>,
    expires: Instant,
    used: Instant,
}

/// Per-host robots and spacing for one process. Clone-cheap.
#[derive(Clone, Default)]
pub struct HostGate {
    inner: Arc<Mutex<HashMap<String, HostEntry>>>,
}

/// `(scheme://host, path?query)` of an http(s) URL.
pub fn split_url(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let rest = rest.split('#').next().unwrap_or("");
    let i = rest.find(['/', '?']).unwrap_or(rest.len());
    let (host, path) = rest.split_at(i);
    if host.is_empty() {
        return None;
    }
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    Some((
        format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()),
        if path.is_empty() { "/".into() } else { path },
    ))
}

impl HostGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a host's robots (tests, or a robots.txt fetched elsewhere), kept for [`ROBOTS_TTL`].
    pub fn insert(&self, origin: &str, robots: Robots) {
        self.insert_for(origin, robots, ROBOTS_TTL);
    }

    fn insert_for(&self, origin: &str, robots: Robots, ttl: Duration) {
        let gap = robots.crawl_delay.unwrap_or(DEFAULT_HOST_GAP).max(DEFAULT_HOST_GAP);
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let origin = origin.to_ascii_lowercase();
        // A refreshed host keeps its schedule unless its crawl delay changed.
        let limiter = match map.get(&origin) {
            Some(e) if e.limiter.limits() == [RateLimit::every(gap)] => e.limiter.clone(),
            _ => Arc::new(Limiter::new(&[RateLimit::every(gap)])),
        };
        if map.len() >= MAX_HOSTS && !map.contains_key(&origin) {
            if let Some(oldest) = map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone()) {
                map.remove(&oldest);
            }
        }
        map.insert(origin, HostEntry { robots: Arc::new(robots), limiter, expires: now + ttl, used: now });
    }

    /// Hosts remembered now.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The robots for `origin`, fetching `/robots.txt` when none is cached or it expired: 2xx
    /// parses and 4xx allows all (both kept [`ROBOTS_TTL`]); anything else (5xx, unreachable)
    /// disallows all for [`ROBOTS_RETRY`]. `None` when cancelled: nothing is cached then, so a
    /// cancelled run cannot leave a host refused for every later fetch.
    pub fn robots(&self, origin: &str, http: &dyn HttpGet, cancel: &CancelToken) -> Option<Arc<Robots>> {
        {
            let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = map.get_mut(origin).filter(|e| e.expires > Instant::now()) {
                e.used = Instant::now();
                return Some(e.robots.clone());
            }
        }
        let req =
            HttpRequest { url: format!("{origin}/robots.txt"), accept: Some("text/plain"), user_agent: USER_AGENT };
        let got = http.get(&req, cancel);
        if cancel.is_cancelled() {
            return None;
        }
        let (robots, ttl) = match got {
            Ok(r) if (200..300).contains(&r.status) => {
                let n = r.bytes.len().min(MAX_ROBOTS_BYTES);
                (Robots::parse(&String::from_utf8_lossy(&r.bytes[..n])), ROBOTS_TTL)
            }
            Ok(r) if (400..500).contains(&r.status) => (Robots::allow_all(), ROBOTS_TTL),
            _ => (Robots::deny_all(), ROBOTS_RETRY),
        };
        let out = Arc::new(robots.clone());
        self.insert_for(origin, robots, ttl);
        Some(out)
    }

    /// Checks robots for `url` and waits for the host's slot. `Ok` means fetch now.
    pub fn admit(&self, url: &str, http: &dyn HttpGet, cancel: &CancelToken) -> Result<(), Refusal> {
        let (origin, path) = split_url(url).ok_or_else(|| Refusal::NotHttp(url.to_string()))?;
        let robots = self.robots(&origin, http, cancel).ok_or(Refusal::Cancelled)?;
        if robots.crawl_delay.is_some_and(|d| d > MAX_CRAWL_DELAY) {
            return Err(Refusal::CrawlDelay { host: origin, delay: robots.crawl_delay.unwrap_or_default() });
        }
        if !robots.allowed(&path) {
            return Err(Refusal::Robots { host: origin, path });
        }
        let limiter = self.inner.lock().unwrap_or_else(|p| p.into_inner()).get(&origin).map(|e| e.limiter.clone());
        match limiter {
            Some(l) if !l.wait(cancel) => Err(Refusal::Cancelled),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::HttpResponse;

    const ROBOTS: &str = "\
User-agent: *
Disallow: /private
Allow: /private/ok
Disallow: /*.pdf$
Crawl-delay: 2

User-agent: BadBot
User-agent: OtherBot
Disallow: /
";

    #[test]
    fn robots_rules_pick_our_group_and_the_longest_match() {
        let r = Robots::parse(ROBOTS);
        assert!(r.allowed("/"));
        assert!(!r.allowed("/private/x"));
        assert!(r.allowed("/private/ok/page"));
        assert!(!r.allowed("/papers/a.pdf"));
        assert!(r.allowed("/papers/a.pdf?download=1"));
        assert_eq!(r.crawl_delay, Some(Duration::from_secs(2)));
        let ours = Robots::parse("User-agent: *\nDisallow: /\n\nUser-agent: MarloweCowork\nDisallow: /tmp\n");
        assert!(ours.allowed("/docs") && !ours.allowed("/tmp/x"), "our own group replaces *");
        assert!(Robots::parse("").allowed("/anything"));
        assert!(!Robots::deny_all().allowed("/"));
    }

    struct Fixed(u16, &'static str);
    impl HttpGet for Fixed {
        fn get(&self, req: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
            assert!(req.url.ends_with("/robots.txt") && req.user_agent == USER_AGENT);
            Ok(HttpResponse {
                status: self.0,
                content_type: Some("text/plain".into()),
                bytes: self.1.as_bytes().to_vec(),
                final_url: req.url.clone(),
                retry_after: None,
            })
        }
    }

    #[test]
    fn the_gate_follows_rfc_9309_status_rules() {
        let cancel = CancelToken::new();
        let g = HostGate::new();
        assert_eq!(
            g.admit("https://a.org/private/x", &Fixed(200, ROBOTS), &cancel),
            Err(Refusal::Robots { host: "https://a.org".into(), path: "/private/x".into() })
        );
        assert!(HostGate::new().admit("https://b.org/x", &Fixed(404, ""), &cancel).is_ok(), "4xx allows all");
        assert!(
            matches!(HostGate::new().admit("https://c.org/x", &Fixed(503, ""), &cancel), Err(Refusal::Robots { .. })),
            "5xx denies all"
        );
        let slow = HostGate::new();
        slow.insert("https://d.org", Robots::parse("User-agent: *\nCrawl-delay: 120\n"));
        assert!(matches!(slow.admit("https://d.org/", &Fixed(500, ""), &cancel), Err(Refusal::CrawlDelay { .. })));
        assert!(matches!(g.admit("ftp://a.org/x", &Fixed(200, ""), &cancel), Err(Refusal::NotHttp(_))));
        assert_eq!(split_url("HTTPS://Ex.org?q=1#f"), Some(("https://ex.org".into(), "/?q=1".into())));
    }

    /// Cancels the token during the robots.txt GET, as `NetHttp` answers "cancelled".
    struct CancelsMidFetch(CancelToken);
    impl HttpGet for CancelsMidFetch {
        fn get(&self, _: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
            self.0.cancel();
            Err("cancelled".into())
        }
    }

    struct Counting(std::sync::atomic::AtomicU32);
    impl HttpGet for Counting {
        fn get(&self, req: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Fixed(404, "").get(req, &CancelToken::new())
        }
    }

    /// Found in review: a cancelled run's robots fetch was cached as "disallow everything" for
    /// the life of the process (arxiv.org refused to every later fetch).
    #[test]
    fn a_cancelled_robots_fetch_is_not_cached_and_entries_expire() {
        let g = HostGate::new();
        let cancel = CancelToken::new();
        let r = g.admit("https://arxiv.org/abs/1", &CancelsMidFetch(cancel.clone()), &cancel);
        assert_eq!(r, Err(Refusal::Cancelled));
        assert!(g.is_empty(), "nothing was cached");
        let live = CancelToken::new();
        let http = Counting(Default::default());
        assert!(g.admit("https://arxiv.org/abs/1", &http, &live).is_ok(), "the next run fetches robots.txt again");
        assert_eq!(http.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        // An unreachable robots.txt denies for a few minutes, not for the life of the process.
        g.insert_for("https://down.org", Robots::deny_all(), Duration::ZERO);
        assert!(g.admit("https://down.org/x", &http, &live).is_ok(), "expired: asked again (and 404 allows)");
        assert_eq!(http.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
