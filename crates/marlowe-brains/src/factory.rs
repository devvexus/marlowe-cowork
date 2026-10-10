//! Provider construction, injected by the binary.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use marlowe_hw::{GpuSource, ModelSource, ServerControl};
use marlowe_model::{ContextAllocations, Provider, ProviderError, ProviderKind};

use crate::credentials::{api_key, CredentialStore};
use crate::error::BrainsError;

/// What a constructor gets. `Debug` never prints the key.
#[derive(Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub api_key: Option<String>,
    /// Override of the provider's default endpoint (tests, remote Ollama).
    pub base_url: Option<String>,
    /// Per-model context windows from the context policies (local providers send them).
    pub context: ContextAllocations,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// Builds a provider from its config.
pub type ProviderCtor =
    Box<dyn Fn(&ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> + Send + Sync>;

/// What the context planner reads and the one thing it may change, injected by the binary.
#[derive(Clone)]
pub struct Hardware {
    pub models: Arc<dyn ModelSource>,
    pub gpu: Arc<dyn GpuSource>,
    pub server: Arc<dyn ServerControl>,
}

/// Registered constructors plus a cache of live providers.
pub struct ProviderFactory {
    ctors: HashMap<ProviderKind, ProviderCtor>,
    base_urls: HashMap<ProviderKind, String>,
    credentials: Arc<dyn CredentialStore>,
    cache: Mutex<HashMap<ProviderKind, Arc<dyn Provider>>>,
    context: ContextAllocations,
    hardware: Option<Hardware>,
}

impl ProviderFactory {
    pub fn new(credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            ctors: HashMap::new(),
            base_urls: HashMap::new(),
            credentials,
            cache: Mutex::new(HashMap::new()),
            context: ContextAllocations::new(),
            hardware: None,
        }
    }

    /// The context planner's sources (Ollama's store and logs, the GPU, the server control).
    /// Without them the planner reports models as unmeasured.
    pub fn set_hardware(&mut self, hw: Hardware) {
        self.hardware = Some(hw);
    }

    pub fn hardware(&self) -> Option<&Hardware> {
        self.hardware.as_ref()
    }

    /// The per-model context windows every provider built here shares.
    pub fn allocations(&self) -> &ContextAllocations {
        &self.context
    }

    /// Registers the constructor for `kind` (called by the binary).
    pub fn register(
        &mut self,
        kind: ProviderKind,
        ctor: impl Fn(&ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> + Send + Sync + 'static,
    ) {
        self.ctors.insert(kind, Box::new(ctor));
        self.invalidate(kind);
    }

    /// Overrides the endpoint passed to `kind`'s constructor.
    pub fn set_base_url(&mut self, kind: ProviderKind, url: impl Into<String>) {
        self.base_urls.insert(kind, url.into());
        self.invalidate(kind);
    }

    pub fn is_registered(&self, kind: ProviderKind) -> bool {
        self.ctors.contains_key(&kind)
    }

    pub fn credentials(&self) -> &Arc<dyn CredentialStore> {
        &self.credentials
    }

    /// The provider for `kind`, constructed on first use and cached.
    pub fn provider(&self, kind: ProviderKind) -> Result<Arc<dyn Provider>, BrainsError> {
        if !kind.is_available_now() {
            return Err(BrainsError::Unsupported(kind));
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = cache.get(&kind) {
            return Ok(p.clone());
        }
        let ctor = self.ctors.get(&kind).ok_or(BrainsError::NotRegistered(kind))?;
        let cfg = ProviderConfig {
            kind,
            api_key: api_key(kind, self.credentials.as_ref()),
            base_url: self.base_urls.get(&kind).cloned(),
            context: self.context.clone(),
        };
        let p = ctor(&cfg)?;
        cache.insert(kind, p.clone());
        Ok(p)
    }

    /// Drops the cached provider so the next use rebuilds it (e.g. after a key change).
    pub fn invalidate(&self, kind: ProviderKind) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).remove(&kind);
    }
}
