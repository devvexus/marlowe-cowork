//! What the research phases need from the outside world, as traits the binary fills in.
//!
//! `marlowe-project` does not depend on `marlowe-exec` (too heavy) or on the daemon's channel
//! wiring. [`Web`] is channel search (the keyless registry), [`PageFetch`] is one page read
//! (robots.txt, per-host spacing, extraction, the JS-render escalation live behind it) and
//! [`Resolver`] answers where a host name points. [`Grant`] is the run-scoped network grant
//! (design decision 1): it sits in front of a [`PageFetch`] and admits a read only when the url is
//! public https that this run found, the host does not resolve to a private address, and quota
//! remains. A redirect is a new request, so the reader is handed an admit callback and must call
//! it for the first url and for every hop before requesting it.

use std::collections::HashSet;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};

use marlowe_extract::Link;
use marlowe_model::CancelToken;
use marlowe_research::{ChannelError, ChannelId, ChannelQuery, SourceRef};

/// Channel search for a project.
pub trait Web: Send + Sync {
    /// The channels that can be searched now.
    fn channels(&self) -> Vec<ChannelId>;

    /// One search on one channel.
    fn search(&self, channel: ChannelId, query: &ChannelQuery, cancel: &CancelToken) -> Result<Vec<SourceRef>, ChannelError>;

    /// Channels that cannot answer, with why (reported as "not searched").
    fn unavailable(&self) -> Vec<(ChannelId, String)> {
        Vec::new()
    }
}

/// A reference list entry a page carries (from `marlowe-extract`'s paper reader).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageRef {
    /// `1` for `[1]`, or a cite key.
    pub label: Option<String>,
    /// The reference's title or text, as printed.
    pub text: String,
    /// DOI.
    pub doi: Option<String>,
    /// arXiv id.
    pub arxiv_id: Option<String>,
    /// A url the reference prints.
    pub url: Option<String>,
    /// Year.
    pub year: Option<u16>,
}

/// Why a page was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// robots.txt disallows it.
    Robots,
    /// The server answered with an error status.
    Http,
    /// The request failed (network, timeout, refused connection).
    Unreachable,
    /// It answered but there is no readable text (binary, empty, script only).
    Unreadable,
    /// The grant refused it (not https, a private address, not found by this run, over quota, a
    /// redirect to such an address).
    Blocked,
}

/// One page read (or the reason it was not).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The url asked for.
    pub url: String,
    /// The url after redirects.
    pub final_url: String,
    /// The page title, when it has one.
    pub title: Option<String>,
    /// Main content as markdown with inline `[anchor](url)` links; empty when unreadable.
    pub text: String,
    /// The page's links, resolved against it.
    pub links: Vec<Link>,
    /// Scholarly identity `<meta>` tags (`citation_title`, `citation_doi`, ...).
    pub meta: Vec<(String, String)>,
    /// The page's reference list, when it is a paper.
    pub references: Vec<PageRef>,
    /// Why there is no text. The words are the harness's (a class), never the server's.
    pub failure: Option<(Failure, String)>,
}

impl Fetched {
    /// A failed read.
    pub fn failed(url: &str, kind: Failure, why: impl Into<String>) -> Fetched {
        Fetched { url: url.into(), final_url: url.into(), failure: Some((kind, why.into())), ..Default::default() }
    }
}

/// What a reader asks before it requests an address: `Ok` to go on, or the reason it may not.
pub type Admit<'a> = &'a (dyn Fn(&str) -> Result<(), String> + Sync);

/// The page reader: politeness, redirects, extraction.
pub trait PageFetch: Send + Sync {
    /// Reads `url`. `admit` is called with `url` and with each redirect target before it is
    /// requested; an `Err` stops the chain and the read ends as [`Failure::Blocked`] with that
    /// reason. A reader that skips this call defeats the grant.
    fn read(&self, url: &str, admit: Admit<'_>, cancel: &CancelToken) -> Fetched;
}

/// Where host names point.
pub trait Resolver: Send + Sync {
    /// The addresses of `host`, or a reason it has none.
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String>;
}

/// The system resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        (host, 443u16).to_socket_addrs().map(|a| a.map(|s| s.ip()).collect()).map_err(|_| "the host name did not resolve".to_string())
    }
}

/// Why the grant refused a url.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

/// The host part of an https url, lower case, with no port.
fn https_host(url: &str) -> Result<String, Refused> {
    let t = url.trim();
    let rest = match t.get(..8) {
        Some(p) if p.eq_ignore_ascii_case("https://") => &t[8..],
        _ => return Err(Refused("only https addresses are read without asking; this one is not https".into())),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(Refused("an address with a user name in it is not read".into()));
    }
    if authority.starts_with('[') {
        return Err(Refused("a raw address is not read without asking".into()));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if port.is_some_and(|p| p != "443" && !p.is_empty()) {
        return Err(Refused("only the standard https port is read without asking".into()));
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(Refused("the address has no host".into()));
    }
    if host.parse::<IpAddr>().is_ok() || host.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(Refused("a raw address is not read without asking".into()));
    }
    let local = host == "localhost" || !host.contains('.') || [".local", ".internal", ".localdomain", ".lan", ".home", ".corp"].iter().any(|s| host.ends_with(s));
    if local {
        return Err(Refused("a private or local host is not read without asking".into()));
    }
    Ok(host)
}

/// The run-scoped network grant: what a project may read without asking.
pub struct Grant {
    inner: Arc<dyn PageFetch>,
    resolver: Arc<dyn Resolver>,
    found: Mutex<HashSet<String>>,
    in_flight: Mutex<u32>,
}

impl Grant {
    /// A grant over `inner`, resolving host names with `resolver`.
    pub fn new(inner: Arc<dyn PageFetch>, resolver: Arc<dyn Resolver>) -> Grant {
        Grant { inner, resolver, found: Mutex::new(HashSet::new()), in_flight: Mutex::new(0) }
    }

    /// Records a url this run found (a search result, a link of a page it fetched, a reference, a
    /// seed). Only such urls are read.
    pub fn found(&self, url: &str) {
        if let (Some(c), Ok(mut set)) = (super::canon(url), self.found.lock()) {
            set.insert(c);
        }
    }

    /// Whether this process has seen the url ([`Grant::found`]).
    pub fn knows(&self, url: &str) -> bool {
        super::canon(url).is_some_and(|c| self.found.lock().map(|s| s.contains(&c)).unwrap_or(false))
    }

    /// Whether `url` may be requested at all: https, a public host name, a standard port. Applied
    /// to the first url and to every redirect hop.
    pub fn check(&self, url: &str) -> Result<(), Refused> {
        let host = https_host(url)?;
        let addrs = self.resolver.resolve(&host).map_err(Refused)?;
        if addrs.is_empty() {
            return Err(Refused("the host name did not resolve".into()));
        }
        if addrs.iter().any(|a| !marlowe_net::is_public(*a)) {
            return Err(Refused("the host name points at a private address".into()));
        }
        Ok(())
    }

    /// Reads `url` under the grant. `remaining` is how many fetches the quota still allows
    /// (the grant's quota less the pages already read); `known_elsewhere` is true when the caller
    /// knows the url came from this run in a way this process did not see (a `hit` item restored
    /// from the journal after a restart).
    pub fn read(&self, url: &str, remaining: u32, known_elsewhere: bool, cancel: &CancelToken) -> Fetched {
        let blocked = |why: &str| Fetched::failed(url, Failure::Blocked, why);
        if !known_elsewhere && !self.knows(url) {
            return blocked("this address was not found by the project, so it is not read without asking");
        }
        if let Err(Refused(why)) = self.check(url) {
            return blocked(&why);
        }
        {
            let Ok(mut n) = self.in_flight.lock() else { return blocked("the fetch counter is unavailable") };
            if *n >= remaining {
                return blocked("the project's fetch quota is used up");
            }
            *n += 1;
        }
        let admit = |hop: &str| self.check(hop).map_err(|r| r.0);
        let mut out = self.inner.read(url, &admit, cancel);
        if let Ok(mut n) = self.in_flight.lock() {
            *n = n.saturating_sub(1);
        }
        // A reader that followed a redirect without asking leaves a final url the grant would
        // have refused: the content is discarded rather than trusted.
        if out.failure.is_none() && out.final_url != out.url && self.check(&out.final_url).is_err() {
            out = blocked("the page redirected to an address that is not read without asking");
        }
        out
    }
}

/// The class word for a failed channel search (never the server's text).
pub fn channel_error_class(e: &ChannelError) -> &'static str {
    match e {
        ChannelError::NotImplemented(_) | ChannelError::NotRegistered(_) | ChannelError::NoLookup { .. } => "not available",
        ChannelError::Status { .. } => "answered with an error",
        ChannelError::RateLimited { .. } => "rate limited",
        ChannelError::Network { .. } => "could not be reached",
        ChannelError::Parse { .. } => "gave an unreadable answer",
        ChannelError::NoBackend { .. } => "has no search backend running",
        ChannelError::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Dns(HashMap<&'static str, Vec<IpAddr>>);
    impl Resolver for Dns {
        fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
            self.0.get(host).cloned().ok_or_else(|| "the host name did not resolve".into())
        }
    }

    /// Follows a scripted redirect chain, calling `admit` for each hop as a real reader must.
    struct Chain {
        hops: Vec<&'static str>,
        asked: Mutex<Vec<String>>,
    }
    impl PageFetch for Chain {
        fn read(&self, url: &str, admit: Admit<'_>, _c: &CancelToken) -> Fetched {
            let mut cur = url.to_string();
            let mut chain = vec![url];
            chain.extend(self.hops.iter().copied());
            for hop in chain {
                if let Err(why) = admit(hop) {
                    return Fetched::failed(url, Failure::Blocked, why);
                }
                self.asked.lock().unwrap().push(hop.to_string());
                cur = hop.to_string();
            }
            Fetched { url: url.into(), final_url: cur, text: "body".into(), ..Default::default() }
        }
    }

    fn dns() -> Arc<dyn Resolver> {
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        let private: IpAddr = "10.0.0.7".parse().unwrap();
        let mut m = HashMap::new();
        m.insert("example.org", vec![public]);
        m.insert("other.example.org", vec![public]);
        m.insert("rebind.example.org", vec![private]);
        m.insert("mixed.example.org", vec![public, private]);
        Arc::new(Dns(m))
    }

    fn grant(hops: Vec<&'static str>) -> (Grant, Arc<Chain>) {
        let chain = Arc::new(Chain { hops, asked: Mutex::new(Vec::new()) });
        let g = Grant::new(chain.clone(), dns());
        for u in ["https://example.org/a", "http://example.org/a", "https://127.0.0.1/a", "https://rebind.example.org/a", "https://localhost/a", "https://mixed.example.org/a", "https://example.org:8443/a"] {
            g.found(u);
        }
        (g, chain)
    }

    fn why(f: &Fetched) -> String {
        match &f.failure {
            Some((Failure::Blocked, w)) => w.clone(),
            other => panic!("expected a block, got {other:?}"),
        }
    }

    #[test]
    fn public_https_the_run_found_is_read() {
        let (g, chain) = grant(vec![]);
        let f = g.read("https://example.org/a", 5, false, &CancelToken::new());
        assert!(f.failure.is_none() && f.text == "body");
        assert_eq!(chain.asked.lock().unwrap().as_slice(), ["https://example.org/a"]);
    }

    #[test]
    fn http_a_raw_ip_a_private_host_and_a_name_that_resolves_to_one_are_refused_before_any_request() {
        let (g, chain) = grant(vec![]);
        let c = CancelToken::new();
        assert!(why(&g.read("http://example.org/a", 5, false, &c)).contains("not https"));
        assert!(why(&g.read("https://127.0.0.1/a", 5, false, &c)).contains("raw address"));
        assert!(why(&g.read("https://localhost/a", 5, false, &c)).contains("private or local"));
        assert!(why(&g.read("https://rebind.example.org/a", 5, false, &c)).contains("private address"), "a public name resolving to 10.0.0.7");
        assert!(why(&g.read("https://mixed.example.org/a", 5, false, &c)).contains("private address"), "one private answer is enough");
        assert!(why(&g.read("https://example.org:8443/a", 5, false, &c)).contains("standard https port"));
        assert!(chain.asked.lock().unwrap().is_empty(), "nothing was requested");
    }

    #[test]
    fn a_url_the_run_did_not_find_is_refused_even_when_it_is_public() {
        let (g, chain) = grant(vec![]);
        let f = g.read("https://example.org/composed-by-a-model", 5, false, &CancelToken::new());
        assert!(why(&f).contains("not found by the project"));
        assert!(chain.asked.lock().unwrap().is_empty());
        // After a restart the caller knows a hit item exists for it.
        assert!(g.read("https://example.org/composed-by-a-model", 5, true, &CancelToken::new()).failure.is_none());
    }

    #[test]
    fn the_quota_is_enforced() {
        let (g, _) = grant(vec![]);
        let f = g.read("https://example.org/a", 0, false, &CancelToken::new());
        assert!(why(&f).contains("quota"));
    }

    #[test]
    fn a_redirect_to_a_refused_host_is_refused_on_that_hop() {
        let (g, chain) = grant(vec!["https://other.example.org/x", "https://rebind.example.org/inner"]);
        let f = g.read("https://example.org/a", 5, false, &CancelToken::new());
        assert!(why(&f).contains("private address"));
        // The first two hops were requested; the private one never was.
        assert_eq!(chain.asked.lock().unwrap().as_slice(), ["https://example.org/a", "https://other.example.org/x"]);
        let (g, chain) = grant(vec!["http://other.example.org/x"]);
        assert!(why(&g.read("https://example.org/a", 5, false, &CancelToken::new())).contains("not https"));
        assert_eq!(chain.asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_reader_that_ignores_the_admit_callback_still_cannot_land_on_a_private_address() {
        struct Careless;
        impl PageFetch for Careless {
            fn read(&self, url: &str, _admit: Admit<'_>, _c: &CancelToken) -> Fetched {
                Fetched { url: url.into(), final_url: "https://rebind.example.org/in".into(), text: "secret".into(), ..Default::default() }
            }
        }
        let g = Grant::new(Arc::new(Careless), dns());
        g.found("https://example.org/a");
        let f = g.read("https://example.org/a", 5, false, &CancelToken::new());
        assert!(f.text.is_empty() && why(&f).contains("redirected"));
    }

    #[test]
    fn user_info_and_empty_hosts_are_refused() {
        for u in ["https://user:pw@example.org/", "https:///x", "https://[::1]/a", "https://intranet/a", "https://printer.local/a", "https://10.1.2.3/"] {
            assert!(https_host(u).is_err(), "{u}");
        }
        assert_eq!(https_host("https://Example.ORG./a?x=1").unwrap(), "example.org");
    }
}
