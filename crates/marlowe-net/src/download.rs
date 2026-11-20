//! Checked downloads: a redirect policy, a streamed file with an incremental sha256, and small
//! JSON-sized reads, over a transport the tests replace.
//!
//! Two users: the memory models (`marlowe-mind::download`, Hugging Face hosts) and the llama.cpp
//! backends (`marlowe-engine-install`, GitHub hosts). Each brings its own [`Policy`]; everything
//! else is shared.
//!
//! A redirect is a second destination: at most [`MAX_HOPS`], https only, and only to the policy's
//! hosts. A file streams to `<file>.part` with an incremental hash and is renamed into place only
//! when the digest matches, so nothing is buffered and a half-written file is never mistaken for
//! a finished one. A bearer token goes to the first host only, never along a redirect.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Redirects followed for one request; the next one is refused.
pub const MAX_HOPS: usize = 5;
/// Longest wait for a read before the transfer is called stalled.
pub const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes between progress callbacks.
const PROGRESS_STEP: u64 = 256 * 1024;

/// Why a download stopped.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// A 404 or 410: the file is not at its URL.
    #[error("not published at {url}")]
    NotPublished { url: String },
    /// A 401 or 403. Hugging Face answers 401 for a repo that does not exist, as for a private one.
    #[error("not public at {url} (HTTP {status})")]
    NotPublic { url: String, status: u16 },
    /// GitHub's limit for unauthenticated calls: when it resets (epoch seconds), if it said.
    #[error("{url}: the rate limit is used up{}", reset.map(|r| format!(" until epoch {r}")).unwrap_or_default())]
    RateLimited { url: String, reset: Option<u64> },
    #[error("{url} answered HTTP {status}")]
    Http { url: String, status: u16 },
    #[error("could not fetch {url}: {detail}")]
    Network { url: String, detail: String },
    /// A redirect (or the URL itself) the policy does not allow.
    #[error("refused {url}: {detail}")]
    Refused { url: String, detail: String },
    #[error("{name}: sha256 {got} does not match the pinned {want}; the file was deleted")]
    Digest { name: String, got: String, want: String },
    #[error("{path}: {detail}")]
    Io { path: String, detail: String },
}

fn io_err(path: &Path, e: std::io::Error) -> DownloadError {
    DownloadError::Io { path: path.display().to_string(), detail: e.to_string() }
}

/// One response. A redirect carries its `Location`; a body is read only for a 200.
pub struct Reply {
    pub status: u16,
    pub redirect_to: Option<String>,
    pub body: Box<dyn Read + Send>,
    /// `X-RateLimit-Remaining` and `-Reset` (epoch seconds), when the server sent them.
    pub rate_remaining: Option<u64>,
    pub rate_reset: Option<u64>,
    /// `Retry-After`, verbatim.
    pub retry_after: Option<String>,
}

impl Reply {
    pub fn new(status: u16, redirect_to: Option<String>, body: Box<dyn Read + Send>) -> Self {
        Self { status, redirect_to, body, rate_remaining: None, rate_reset: None, retry_after: None }
    }
}

/// What a request asks for besides the URL.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ask<'a> {
    /// `Accept`; `*/*` when empty.
    pub accept: &'a str,
    /// A bearer token for the first host only.
    pub bearer: Option<&'a str>,
}

/// One GET that follows no redirect. The real one is [`Net`]; tests supply fixtures.
pub trait Transport {
    fn get(&self, url: &str) -> Result<Reply, String>;

    /// [`Transport::get`] with an `Accept` and a token. A fixture that ignores both is fine.
    fn get_as(&self, url: &str, _ask: &Ask<'_>) -> Result<Reply, String> {
        self.get(url)
    }
}

/// The workspace's rustls client.
pub struct Net;

struct Body(crate::ResponseStream);

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.body().read(buf)
    }
}

impl Transport for Net {
    fn get(&self, url: &str) -> Result<Reply, String> {
        self.get_as(url, &Ask::default())
    }

    fn get_as(&self, url: &str, ask: &Ask<'_>) -> Result<Reply, String> {
        let target = crate::Target::parse(url).map_err(|e| e.to_string())?;
        let headers = crate::Headers { user_agent: "marlowe", accept: if ask.accept.is_empty() { "*/*" } else { ask.accept } };
        let auth = ask.bearer.map(|t| format!("Bearer {t}"));
        let extra: Vec<(&str, &str)> = auth.iter().map(|a| ("Authorization", a.as_str())).collect();
        let r = crate::shared().get_streaming_with(&target, &headers, &extra, READ_TIMEOUT).map_err(|e| e.to_string())?;
        Ok(Reply {
            status: r.status,
            redirect_to: r.redirect_to.clone(),
            rate_remaining: r.rate_remaining,
            rate_reset: r.rate_reset,
            retry_after: r.retry_after.clone(),
            body: Box::new(Body(r)),
        })
    }
}

/// Where a request may go.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// `example.com` and every subdomain of it, on 443 only.
    roots: Vec<String>,
    /// These hosts on 443 only.
    hosts: Vec<String>,
    /// These hosts on any port (a configured base URL).
    any_port: Vec<String>,
}

impl Policy {
    pub fn new(roots: &[&str], hosts: &[&str]) -> Self {
        Self { roots: roots.iter().map(|s| s.to_string()).collect(), hosts: hosts.iter().map(|s| s.to_string()).collect(), any_port: Vec::new() }
    }

    /// Also allows `host` on any port.
    pub fn with_any_port(mut self, host: Option<String>) -> Self {
        self.any_port.extend(host);
        self
    }

    /// Whether `host` is under one of the roots.
    pub fn under_root(&self, host: &str) -> bool {
        self.roots.iter().any(|r| host == r || host.strip_suffix(r.as_str()).is_some_and(|p| p.ends_with('.')))
    }

    fn host_ok(&self, host: &str, port: u16) -> bool {
        (port == 443 && (self.under_root(host) || self.hosts.iter().any(|h| h == host))) || self.any_port.iter().any(|h| h == host)
    }

    /// `url`'s host if it is https and allowed.
    pub fn check(&self, url: &str) -> Result<String, DownloadError> {
        let refused = |detail: String| DownloadError::Refused { url: url.to_string(), detail };
        let t = crate::Target::parse(url).map_err(|e| match e {
            crate::FetchError::Unsupported { detail, .. } => refused(detail),
            other => refused(other.to_string()),
        })?;
        if !self.host_ok(&t.host, t.port) {
            return Err(refused(format!("{}:{} is not an allowed host", t.host, t.port)));
        }
        Ok(t.host)
    }
}

/// An absolute https `Location`, or a path on the current host; anything else is refused.
fn resolve(host: &str, location: &str) -> Result<String, DownloadError> {
    let refused = |detail: &str| DownloadError::Refused { url: location.to_string(), detail: detail.to_string() };
    if location.starts_with("//") {
        return Err(refused("a scheme-relative redirect"));
    }
    if location.starts_with('/') {
        return Ok(format!("https://{host}{location}"));
    }
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    Err(refused("an unusable redirect"))
}

/// Opens `url`, following at most [`MAX_HOPS`] allowed redirects.
pub fn open(t: &dyn Transport, policy: &Policy, url: &str, ask: &Ask<'_>) -> Result<Reply, DownloadError> {
    let mut url = url.to_string();
    let mut ask = *ask;
    for hop in 0..=MAX_HOPS {
        let host = policy.check(&url)?;
        let reply = t.get_as(&url, &ask).map_err(|detail| DownloadError::Network { url: url.clone(), detail })?;
        let redirect = matches!(reply.status, 301 | 302 | 303 | 307 | 308);
        match (redirect, reply.redirect_to.as_deref()) {
            (true, Some(loc)) => {
                if hop == MAX_HOPS {
                    return Err(DownloadError::Refused { url, detail: format!("more than {MAX_HOPS} redirects") });
                }
                url = resolve(&host, loc)?;
                // A token belongs to the host it was given for.
                ask.bearer = None;
            }
            (true, None) => return Err(DownloadError::Http { url, status: reply.status }),
            _ => return Ok(reply),
        }
    }
    unreachable!("the last hop returns")
}

/// The body of a 200 read whole, at most `cap` bytes (a JSON answer, not a file).
pub fn get_bytes(t: &dyn Transport, policy: &Policy, url: &str, ask: &Ask<'_>, cap: usize) -> Result<Vec<u8>, DownloadError> {
    let mut reply = open(t, policy, url, ask)?;
    check_status(&reply, url)?;
    let mut out = Vec::new();
    reply
        .body
        .by_ref()
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| DownloadError::Network { url: url.to_string(), detail: e.to_string() })?;
    if out.len() > cap {
        return Err(DownloadError::Network { url: url.to_string(), detail: format!("the answer is larger than {cap} bytes") });
    }
    Ok(out)
}

fn check_status(reply: &Reply, url: &str) -> Result<(), DownloadError> {
    match reply.status {
        200 => Ok(()),
        404 | 410 => Err(DownloadError::NotPublished { url: url.to_string() }),
        // GitHub says 403 (or 429) with a zero remaining count when the limit is spent.
        403 | 429 if reply.rate_remaining == Some(0) || reply.retry_after.is_some() || reply.status == 429 => {
            Err(DownloadError::RateLimited { url: url.to_string(), reset: reply.rate_reset })
        }
        status @ (401 | 403) => Err(DownloadError::NotPublic { url: url.to_string(), status }),
        status => Err(DownloadError::Http { url: url.to_string(), status }),
    }
}

/// The lower-case hex sha256 of some bytes.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The lower-case hex sha256 of a file.
pub fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(format!("{:x}", h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// Streams `url` into `part`; returns the sha256 of what arrived. `on` gets the bytes so far.
pub fn fetch_to(
    url: &str,
    part: &Path,
    t: &dyn Transport,
    policy: &Policy,
    on: &mut dyn FnMut(u64),
) -> Result<String, DownloadError> {
    let mut reply = open(t, policy, url, &Ask::default())?;
    check_status(&reply, url)?;
    let mut out = std::fs::File::create(part).map_err(|err| io_err(part, err))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let (mut n, mut reported) = (0u64, 0u64);
    loop {
        let k = reply
            .body
            .read(&mut buf)
            .map_err(|err| DownloadError::Network { url: url.to_string(), detail: err.to_string() })?;
        if k == 0 {
            break;
        }
        hash.update(&buf[..k]);
        out.write_all(&buf[..k]).map_err(|err| io_err(part, err))?;
        n += k as u64;
        if n - reported >= PROGRESS_STEP {
            reported = n;
            on(n);
        }
    }
    out.flush().map_err(|err| io_err(part, err))?;
    Ok(format!("{:x}", hash.finalize()))
}

/// Fetches `url` to `path` and checks it against `want` (lower-case hex sha256): through
/// `<path>.part`, renamed into place on a match, deleted on a mismatch. A file already at `path`
/// with that digest is kept and nothing is fetched. `name` is how the messages call the file.
pub fn fetch_verified(
    url: &str,
    path: &Path,
    want: &str,
    name: &str,
    t: &dyn Transport,
    policy: &Policy,
    on: &mut dyn FnMut(u64),
) -> Result<(), DownloadError> {
    if path.is_file() && sha256_of(path).is_ok_and(|h| h.eq_ignore_ascii_case(want)) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| io_err(dir, err))?;
    }
    let part = path.with_file_name(format!("{}.part", path.file_name().and_then(|n| n.to_str()).unwrap_or("download")));
    let got = match fetch_to(url, &part, t, policy, on) {
        Ok(h) => h,
        Err(err) => {
            let _ = std::fs::remove_file(&part);
            return Err(err);
        }
    };
    if !got.eq_ignore_ascii_case(want) {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::Digest { name: name.to_string(), got, want: want.to_string() });
    }
    std::fs::rename(&part, path).map_err(|err| io_err(path, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake(Mutex<HashMap<String, (u16, Option<String>, Vec<u8>)>>, Mutex<Vec<(String, bool)>>);

    impl Fake {
        fn route(self, url: &str, status: u16, loc: Option<&str>, body: &[u8]) -> Self {
            self.0.lock().unwrap().insert(url.into(), (status, loc.map(Into::into), body.to_vec()));
            self
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str) -> Result<Reply, String> {
            self.get_as(url, &Ask::default())
        }
        fn get_as(&self, url: &str, ask: &Ask<'_>) -> Result<Reply, String> {
            self.1.lock().unwrap().push((url.into(), ask.bearer.is_some()));
            let r = self.0.lock().unwrap();
            let (s, l, b) = r.get(url).ok_or_else(|| format!("no route for {url}"))?.clone();
            Ok(Reply::new(s, l, Box::new(Cursor::new(b))))
        }
    }

    fn gh() -> Policy {
        Policy::new(&[], &["github.com", "api.github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"])
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-net-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn github_hosts_pass_and_look_alikes_do_not() {
        let p = gh();
        for ok in ["https://github.com/a", "https://api.github.com/x", "https://objects.githubusercontent.com/y", "https://release-assets.githubusercontent.com/z"] {
            assert!(p.check(ok).is_ok(), "{ok}");
        }
        for bad in ["https://github.com.evil.example/a", "https://evilgithub.com/a", "https://raw.githubusercontent.com/a", "https://github.com:8443/a", "http://github.com/a"] {
            assert!(p.check(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_token_is_sent_to_the_first_host_and_dropped_on_a_redirect() {
        let fake = Fake::default()
            .route("https://api.github.com/r", 302, Some("https://objects.githubusercontent.com/o"), b"")
            .route("https://objects.githubusercontent.com/o", 200, None, b"{}");
        let got = get_bytes(&fake, &gh(), "https://api.github.com/r", &Ask { accept: "x", bearer: Some("tok") }, 100).unwrap();
        assert_eq!(got, b"{}");
        assert_eq!(*fake.1.lock().unwrap(), vec![("https://api.github.com/r".to_string(), true), ("https://objects.githubusercontent.com/o".to_string(), false)]);
    }

    #[test]
    fn a_spent_rate_limit_is_named_and_an_oversize_answer_refused() {
        struct Limited;
        impl Transport for Limited {
            fn get(&self, _: &str) -> Result<Reply, String> {
                let mut r = Reply::new(403, None, Box::new(std::io::empty()));
                r.rate_remaining = Some(0);
                r.rate_reset = Some(1_700_000_000);
                Ok(r)
            }
        }
        let e = get_bytes(&Limited, &gh(), "https://api.github.com/r", &Ask::default(), 10).unwrap_err();
        assert!(matches!(e, DownloadError::RateLimited { reset: Some(1_700_000_000), .. }), "{e}");
        let fake = Fake::default().route("https://api.github.com/big", 200, None, &[b'x'; 50]);
        assert!(get_bytes(&fake, &gh(), "https://api.github.com/big", &Ask::default(), 10).is_err());
    }

    #[test]
    fn a_verified_file_lands_only_on_a_matching_digest() {
        let dir = scratch("verified");
        let body = b"zip bytes".to_vec();
        let url = "https://github.com/o/r/releases/download/t/a.zip";
        let fake = Fake::default().route(url, 200, None, &body);
        let want = format!("{:x}", Sha256::digest(&body));
        let path = dir.join("a.zip");
        let bad = "0".repeat(64);
        let e = fetch_verified(url, &path, &bad, "a.zip", &fake, &gh(), &mut |_| {}).unwrap_err();
        assert!(matches!(e, DownloadError::Digest { .. }));
        assert!(!path.exists() && !dir.join("a.zip.part").exists(), "a mismatch leaves nothing behind");
        fetch_verified(url, &path, &want, "a.zip", &fake, &gh(), &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        let asked = fake.1.lock().unwrap().len();
        fetch_verified(url, &path, &want, "a.zip", &fake, &gh(), &mut |_| {}).unwrap();
        assert_eq!(fake.1.lock().unwrap().len(), asked, "a file already there with the digest is not fetched again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
