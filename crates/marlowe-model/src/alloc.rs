//! The context window each local model is allocated with, shared between the library (which
//! owns the per-model policy) and the provider (which sends it as `num_ctx`).
//!
//! One number per model, the same on every request: a different `num_ctx` makes Ollama reload
//! the runner (6–14 s on a 27B model), so an agent's smaller window is enforced in context
//! assembly, never by asking for a smaller allocation.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Model id → allocated context (tokens). Cheap to clone; every clone sees every update.
#[derive(Debug, Clone, Default)]
pub struct ContextAllocations {
    inner: Arc<RwLock<HashMap<String, u32>>>,
}

impl ContextAllocations {
    pub fn new() -> Self {
        Self::default()
    }

    /// The allocation for a provider-native model id, when a policy set one.
    pub fn get(&self, model_id: &str) -> Option<u32> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).get(model_id).copied()
    }

    pub fn set(&self, model_id: &str, tokens: u32) {
        self.inner.write().unwrap_or_else(|e| e.into_inner()).insert(model_id.to_string(), tokens);
    }

    pub fn remove(&self, model_id: &str) {
        self.inner.write().unwrap_or_else(|e| e.into_inner()).remove(model_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_updates() {
        let a = ContextAllocations::new();
        let b = a.clone();
        a.set("m", 47_104);
        assert_eq!(b.get("m"), Some(47_104));
        b.remove("m");
        assert_eq!(a.get("m"), None);
    }
}
