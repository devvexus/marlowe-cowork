//! Small shared pieces: URLs, percent and form encoding, base64url, random strings.

use std::fmt;

use sha2::{Digest, Sha256};

/// A parsed `http(s)` URL. No userinfo is accepted: credentials never travel in a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, String> {
        let s = s.trim();
        let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("{s:?} is not an http(s) URL"));
        };
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        let path = path.split('#').next().unwrap_or("/").to_string();
        if authority.contains('@') {
            return Err("a URL with userinfo is not accepted".into());
        }
        let default_port = if https { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, tail) = v6.split_once(']').ok_or("unterminated IPv6 host")?;
            let port = match tail.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| "the port is not a number".to_string())?,
                None => default_port,
            };
            (h.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "the port is not a number".to_string())?),
                None => (authority.to_string(), default_port),
            }
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err("the host is empty or contains whitespace".into());
        }
        Ok(Url { https, host: host.to_ascii_lowercase(), port, path })
    }

    /// `localhost`, `127.0.0.0/8` and `::1`: a server on this machine.
    pub fn is_loopback(&self) -> bool {
        self.host == "localhost"
            || self.host == "::1"
            || self.host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
    }

    /// `scheme://host[:port]` with the default port elided.
    pub fn origin(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == default {
            format!("{}://{host}", self.scheme())
        } else {
            format!("{}://{host}:{}", self.scheme(), self.port)
        }
    }

    pub fn scheme(&self) -> &'static str {
        if self.https {
            "https"
        } else {
            "http"
        }
    }

    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or("/")
    }

    /// This URL with `path` (and query) replaced.
    pub fn with_path(&self, path: &str) -> Url {
        Url { path: if path.starts_with('/') { path.to_string() } else { format!("/{path}") }, ..self.clone() }
    }

    /// Resolves `reference` (absolute URL, absolute path or relative path) against this URL.
    pub fn join(&self, reference: &str) -> Result<Url, String> {
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return Url::parse(reference);
        }
        if reference.starts_with('/') {
            return Ok(self.with_path(reference));
        }
        let base = self.path_only();
        let dir = &base[..base.rfind('/').map_or(0, |i| i + 1)];
        Ok(self.with_path(&format!("{dir}{reference}")))
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.origin(), self.path)
    }
}

/// RFC 3986 percent-encoding of everything but unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `a=b&c=d` with both sides percent-encoded.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v))).collect::<Vec<_>>().join("&")
}

pub fn form_decode(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

/// Unpadded base64url (RFC 4648 section 5).
pub fn base64url(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

pub fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// `n` random bytes, base64url.
pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if getrandom::getrandom(&mut buf).is_err() {
        // No entropy source: derive from the clock and a counter. Only reachable when the OS
        // refuses randomness, in which case nothing else in the process works either.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let seed = Sha256::digest(format!("{t}:{c}"));
        for (i, b) in buf.iter_mut().enumerate() {
            *b = seed[i % 32];
        }
    }
    base64url(&buf)
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_and_resolve() {
        let u = Url::parse("https://Example.com/mcp/v1?x=1").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (true, "example.com", 443, "/mcp/v1?x=1"));
        assert_eq!(u.origin(), "https://example.com");
        assert_eq!(u.join("/a/b").unwrap().to_string(), "https://example.com/a/b");
        assert_eq!(u.join("other").unwrap().to_string(), "https://example.com/mcp/other");
        assert_eq!(u.join("http://127.0.0.1:9/x").unwrap().port, 9);
        let l = Url::parse("http://127.0.0.1:8080").unwrap();
        assert!(l.is_loopback());
        assert_eq!(l.path, "/");
        assert!(Url::parse("http://[::1]:9/").unwrap().is_loopback());
        assert!(!Url::parse("https://example.com").unwrap().is_loopback());
        assert!(Url::parse("https://u:p@example.com").is_err());
        assert!(Url::parse("ftp://example.com").is_err());
    }

    #[test]
    fn encodings_round_trip() {
        assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(percent_decode("a%20b%26c%3Dd%2F%C3%A9+x"), "a b&c=d/é x");
        assert_eq!(percent_decode("100%"), "100%");
        let f = form_encode(&[("code", "a b"), ("state", "x&y")]);
        assert_eq!(form_decode(&f), vec![("code".into(), "a b".into()), ("state".into(), "x&y".into())]);
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
    }
}
