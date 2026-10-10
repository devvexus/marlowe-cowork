//! API keys: Windows Credential Manager (via `keyring`), environment fallback, and the list of
//! variables scrubbed from every child process.

use std::collections::HashMap;
use std::sync::Mutex;

use marlowe_model::ProviderKind;

use crate::error::BrainsError;

/// Environment variables removed from every child process the harness spawns. The list lives in
/// `marlowe_os::child`, which applies it to every launch; this re-export keeps old callers working.
pub use marlowe_os::child::SCRUBBED_ENV;

/// The variable names to remove from a child process environment.
pub fn scrubbed_env() -> &'static [&'static str] {
    SCRUBBED_ENV
}

/// The environment variable a provider's key may come from.
pub fn env_var(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::OpenRouter => Some("OPENROUTER_API_KEY"),
        ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
        ProviderKind::OpenAi => Some("OPENAI_API_KEY"),
        ProviderKind::Ollama | ProviderKind::LocalGguf => None,
    }
}

/// Where API keys are kept. Implementations never log or persist a secret elsewhere.
pub trait CredentialStore: Send + Sync {
    fn get(&self, kind: ProviderKind) -> Result<Option<String>, BrainsError>;
    fn set(&self, kind: ProviderKind, secret: &str) -> Result<(), BrainsError>;
    fn delete(&self, kind: ProviderKind) -> Result<(), BrainsError>;
}

/// The key for `kind`: a stored credential first, else its environment variable.
pub fn api_key(kind: ProviderKind, store: &dyn CredentialStore) -> Option<String> {
    if let Ok(Some(k)) = store.get(kind) {
        if !k.trim().is_empty() {
            return Some(k);
        }
    }
    env_var(kind)
        .and_then(|v| std::env::var(v).ok())
        .filter(|k| !k.trim().is_empty())
}

/// Windows Credential Manager (generic credentials), one entry per provider.
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    /// The credential service name.
    pub const SERVICE: &'static str = "MarloweCowork";

    pub fn new() -> Self {
        Self::with_service(Self::SERVICE)
    }

    /// A store under another service name (tests).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn entry(&self, kind: ProviderKind) -> Result<keyring::Entry, BrainsError> {
        keyring::Entry::new(&self.service, kind.as_str()).map_err(cred_err)
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

// keyring errors never contain the password.
fn cred_err(e: keyring::Error) -> BrainsError {
    BrainsError::Credential(e.to_string())
}

impl CredentialStore for KeyringStore {
    fn get(&self, kind: ProviderKind) -> Result<Option<String>, BrainsError> {
        match self.entry(kind)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(cred_err(e)),
        }
    }

    fn set(&self, kind: ProviderKind, secret: &str) -> Result<(), BrainsError> {
        self.entry(kind)?.set_password(secret).map_err(cred_err)
    }

    fn delete(&self, kind: ProviderKind) -> Result<(), BrainsError> {
        match self.entry(kind)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(cred_err(e)),
        }
    }
}

/// In-memory store for tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    keys: Mutex<HashMap<ProviderKind, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, kind: ProviderKind) -> Result<Option<String>, BrainsError> {
        Ok(self.keys.lock().unwrap_or_else(|e| e.into_inner()).get(&kind).cloned())
    }

    fn set(&self, kind: ProviderKind, secret: &str) -> Result<(), BrainsError> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).insert(kind, secret.to_string());
        Ok(())
    }

    fn delete(&self, kind: ProviderKind) -> Result<(), BrainsError> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).remove(&kind);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_key_wins_and_every_provider_key_is_scrubbed() {
        let s = MemoryStore::new();
        assert_eq!(api_key(ProviderKind::Ollama, &s), None);
        s.set(ProviderKind::OpenRouter, "sk-or-1").unwrap();
        assert_eq!(api_key(ProviderKind::OpenRouter, &s).as_deref(), Some("sk-or-1"));
        for k in [ProviderKind::OpenRouter, ProviderKind::Anthropic, ProviderKind::OpenAi] {
            assert!(scrubbed_env().contains(&env_var(k).unwrap()));
        }
    }
}
