//! Provider detection: a running Ollama, keys in the environment or the credential store.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use marlowe_model::ProviderKind;
use marlowe_proto::ProviderStatus;

use crate::credentials::{api_key, env_var, CredentialStore};

/// Where Ollama listens by default.
pub const OLLAMA_ADDR: &str = "127.0.0.1:11434";

/// True when an Ollama server answers `GET /api/tags` at `addr` within `timeout`.
pub fn detect_ollama(addr: &str, timeout: Duration) -> bool {
    let Some(sock) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&sock, timeout) else { return false };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let req = format!("GET /api/tags HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 16];
    let mut got = 0;
    while got < buf.len() {
        match s.read(&mut buf[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    buf[..got].starts_with(b"HTTP/1.") && buf[..got].windows(4).any(|w| w == b" 200")
}

/// Status of every provider in `ProviderKind::ALL` order.
pub fn detect_providers(credentials: &dyn CredentialStore) -> Vec<ProviderStatus> {
    ProviderKind::ALL
        .iter()
        .map(|&kind| status(kind, credentials, || detect_ollama(OLLAMA_ADDR, Duration::from_millis(400))))
        .collect()
}

fn status(kind: ProviderKind, creds: &dyn CredentialStore, ollama: impl FnOnce() -> bool) -> ProviderStatus {
    let available_now = kind.is_available_now();
    let (configured, detected, note) = match kind {
        ProviderKind::Ollama => {
            let up = available_now && ollama();
            (up, up, (!up).then(|| "not running at 127.0.0.1:11434".to_string()))
        }
        _ if !available_now => (false, false, Some("coming soon".to_string())),
        _ => {
            let stored = matches!(creds.get(kind), Ok(Some(s)) if !s.is_empty());
            let in_env = env_var(kind).is_some_and(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()));
            let key = stored || in_env || api_key(kind, creds).is_some();
            (key, key, (!key).then(|| "no API key".to_string()))
        }
    };
    ProviderStatus { kind, available_now, configured, detected, note }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use std::net::TcpListener;

    #[test]
    fn detects_a_fake_ollama_and_a_dead_port() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut b = [0u8; 256];
            let _ = s.read(&mut b);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        });
        assert!(detect_ollama(&addr, Duration::from_secs(2)));
        t.join().unwrap();
        assert!(!detect_ollama(&addr, Duration::from_millis(300)));
    }

    #[test]
    fn statuses_cover_all_kinds_in_order() {
        let store = MemoryStore::new();
        store.set(ProviderKind::OpenRouter, "sk-test").unwrap();
        let all: Vec<_> = ProviderKind::ALL.iter().map(|&k| status(k, &store, || true)).collect();
        assert_eq!(all.len(), 5);
        assert!(all[0].detected && all[0].available_now);
        let or = all.iter().find(|s| s.kind == ProviderKind::OpenRouter).unwrap();
        assert!(or.configured && or.detected);
        let ant = all.iter().find(|s| s.kind == ProviderKind::Anthropic).unwrap();
        assert!(!ant.available_now && !ant.detected && ant.note.as_deref() == Some("coming soon"));
    }
}
