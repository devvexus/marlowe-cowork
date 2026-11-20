//! Provider detection: local model files on disk (Ollama's store or the Hugging Face cache; no
//! model server need run: Marlowe serves them itself), keys in the environment or the credential
//! store.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
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

/// Ollama's model store: `OLLAMA_MODELS`, else `~/.ollama/models` (the same rule as
/// `marlowe_provider::ollama_store::store_root`).
pub fn ollama_store() -> PathBuf {
    if let Some(v) = std::env::var_os("OLLAMA_MODELS").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    let home = ["USERPROFILE", "HOME"].iter().find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty())).map(PathBuf::from);
    home.unwrap_or_else(|| PathBuf::from(".")).join(".ollama").join("models")
}

/// The Hugging Face hub cache: `HF_HUB_CACHE`, else `HF_HOME\hub`, else `~\.cache\huggingface\hub`
/// (the same rule as `marlowe_provider::sources::hf_hubs`).
pub fn hf_hub() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(p) = var("HF_HUB_CACHE").or_else(|| var("HUGGINGFACE_HUB_CACHE")) {
        return p;
    }
    if let Some(h) = var("HF_HOME") {
        return h.join("hub");
    }
    var("USERPROFILE").or_else(|| var("HOME")).unwrap_or_else(|| PathBuf::from(".")).join(".cache").join("huggingface").join("hub")
}

/// Status of every provider in `ProviderKind::ALL` order.
pub fn detect_providers(credentials: &dyn CredentialStore) -> Vec<ProviderStatus> {
    let (store, hub) = (ollama_store(), hf_hub());
    let local = || {
        (store.join("manifests").is_dir() || hub.is_dir()).then_some(()).ok_or_else(|| format!("{} or {}", store.display(), hub.display()))
    };
    ProviderKind::ALL.iter().map(|&kind| status(kind, credentials, local)).collect()
}

/// `ollama` is whether local model files are there: Ollama's store or the Hugging Face cache
/// (`Err` names where they were looked for; folders added by the user are not known here).
fn status(kind: ProviderKind, creds: &dyn CredentialStore, ollama: impl FnOnce() -> Result<(), String>) -> ProviderStatus {
    let available_now = kind.is_available_now();
    let (configured, detected, note) = match kind {
        ProviderKind::Ollama => match ollama() {
            Ok(()) => (available_now, available_now, None),
            Err(at) => (false, false, Some(format!("no Ollama model store or Hugging Face cache at {at}"))),
        },
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
        let all: Vec<_> = ProviderKind::ALL.iter().map(|&k| status(k, &store, || Ok(()))).collect();
        assert_eq!(all.len(), 4);
        assert!(all[0].detected && all[0].available_now);
        let or = all.iter().find(|s| s.kind == ProviderKind::OpenRouter).unwrap();
        assert!(or.configured && or.detected);
        let ant = all.iter().find(|s| s.kind == ProviderKind::Anthropic).unwrap();
        assert!(!ant.available_now && !ant.detected && ant.note.as_deref() == Some("coming soon"));
        // No server need run: model files on disk are what count.
        let none = status(ProviderKind::Ollama, &store, || Err("C:/x/models".into()));
        assert!(!none.detected && none.note.as_deref() == Some("no Ollama model store or Hugging Face cache at C:/x/models"));
    }
}
