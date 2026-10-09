//! The Ollama model store: `GET /api/tags` only. Never inference, never pull.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::SentinelError;

/// Largest `/api/tags` response read.
const MAX_BODY: u64 = 8 * 1024 * 1024;

/// One tag from `/api/tags`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaModel {
    pub name: String,
    pub digest: String,
    /// `modified_at` as Ollama reports it.
    pub modified_at: String,
    pub size: u64,
}

/// Fetches the tag list over plain loopback HTTP/1.0 (so the reply is never chunked).
pub fn tags(addr: SocketAddr, timeout: Duration) -> Result<Vec<OllamaModel>, SentinelError> {
    let err = |e: &dyn std::fmt::Display| SentinelError::Ollama(e.to_string());
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| err(&e))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| err(&e))?;
    s.set_write_timeout(Some(timeout)).map_err(|e| err(&e))?;
    // One write, so the request goes out as a single segment.
    let req = format!("GET /api/tags HTTP/1.0\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).map_err(|e| err(&e))?;
    let mut raw = Vec::new();
    s.take(MAX_BODY).read_to_end(&mut raw).map_err(|e| err(&e))?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| err(&"malformed response"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let status_ok = head.lines().next().is_some_and(|l| l.split_whitespace().nth(1) == Some("200"));
    if !status_ok {
        return Err(err(&head.lines().next().unwrap_or("no status").to_string()));
    }
    let mut body = raw[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    parse_tags(&body)
}

/// Decodes an HTTP/1.1 chunked body (defensive: HTTP/1.0 replies should not use it).
fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(nl) = b.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&b[..nl]).ok().and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok());
        let Some(size) = size.filter(|n| *n > 0) else { break };
        let start = nl + 2;
        let end = (start + size).min(b.len());
        out.extend_from_slice(&b[start..end]);
        b = b.get(end + 2..).unwrap_or(&[]);
    }
    out
}

/// Parses an `/api/tags` body: `{"models":[{"name","digest","modified_at","size"}, ...]}`.
pub fn parse_tags(body: &[u8]) -> Result<Vec<OllamaModel>, SentinelError> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| SentinelError::Ollama(e.to_string()))?;
    let models = v.get("models").and_then(|m| m.as_array()).ok_or_else(|| SentinelError::Ollama("no models list".into()))?;
    let s = |m: &serde_json::Value, k: &str| m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok(models
        .iter()
        .map(|m| OllamaModel {
            name: if s(m, "name").is_empty() { s(m, "model") } else { s(m, "name") },
            digest: s(m, "digest"),
            modified_at: s(m, "modified_at"),
            size: m.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
        })
        .filter(|m| !m.name.is_empty())
        .collect())
}

/// Names new or updated (digest changed) in `now` relative to `before`, sorted.
pub fn changed(before: &[OllamaModel], now: &[OllamaModel]) -> Vec<String> {
    let mut out: Vec<String> = now
        .iter()
        .filter(|m| !before.iter().any(|b| b.name == m.name && b.digest == m.digest))
        .map(|m| m.name.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn model(name: &str, digest: &str) -> OllamaModel {
        OllamaModel { name: name.into(), digest: digest.into(), modified_at: String::new(), size: 1 }
    }

    #[test]
    fn diffs_by_name_and_digest() {
        let before = vec![model("qwen3:8b", "a"), model("marlowe-valiance-22b-wnr4:latest", "b")];
        let now = vec![model("qwen3:8b", "a"), model("marlowe-valiance-22b-wnr4:latest", "c"), model("marlowe-valiance-22b-wnr5:latest", "d")];
        assert_eq!(changed(&before, &now), vec!["marlowe-valiance-22b-wnr4:latest", "marlowe-valiance-22b-wnr5:latest"]);
        assert!(changed(&now, &now).is_empty());
    }

    #[test]
    fn fetches_tags_from_a_local_stub() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for chunked in [false, true] {
                let (mut s, _) = listener.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 256];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "client closed early");
                    req.extend_from_slice(&buf[..n]);
                }
                let req = String::from_utf8_lossy(&req).to_string();
                assert!(req.starts_with("GET /api/tags HTTP/1.0"), "{req}");
                let body = r#"{"models":[{"name":"marlowe-x:latest","model":"marlowe-x:latest","modified_at":"2026-10-09T05:00:00Z","size":42,"digest":"abc"}]}"#;
                let reply = if chunked {
                    let (a, b) = body.split_at(20);
                    format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n", a.len(), b.len())
                } else {
                    format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}")
                };
                s.write_all(reply.as_bytes()).unwrap();
            }
        });
        for _ in 0..2 {
            let got = tags(addr, Duration::from_secs(5)).unwrap();
            assert_eq!(got, vec![OllamaModel { name: "marlowe-x:latest".into(), digest: "abc".into(), modified_at: "2026-10-09T05:00:00Z".into(), size: 42 }]);
        }
        server.join().unwrap();
        // Nothing listening: an error, never a panic.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(tags(closed, Duration::from_millis(500)).is_err());
    }
}
