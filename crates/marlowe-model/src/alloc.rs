//! The context window each local model is allocated with, shared between the library (which
//! owns the per-model policy) and the provider (which sends it as `num_ctx`).
//!
//! One number per model, the same on every request: a different `num_ctx` makes Ollama reload
//! the runner (6–14 s on a 27B model), so an agent's smaller window is enforced in context
//! assembly, never by asking for a smaller allocation.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// How Marlowe's own llama-server serves one model: `slots` parallel conversations of
/// `ctx_per_slot` tokens each (`-c` is their product), with the KV cache at type `kv`
/// (`f16`, `q8_0` or `q4_0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineShape {
    pub slots: u16,
    pub ctx_per_slot: u32,
    pub kv: String,
}

/// Decides, per model, whether Marlowe's own llama-server serves it (and how) or Ollama does.
/// The library implements it over the planner; the Ollama provider asks it on every request.
pub trait EngineAdvisor: Send + Sync {
    /// The shape for `model_id`, or `None` when Ollama serves the model.
    fn shape(&self, model_id: &str) -> Option<EngineShape>;

    /// GPU memory the planner expects the shape of `model_id` to take (weights, every slot's cache
    /// and compute), MiB. The engine checks it against the memory free when it starts the server.
    fn need_mib(&self, _model_id: &str) -> Option<f64> {
        None
    }

    /// The engine did not run `model_id` after all, and why (it did not fit, or the driver spilled
    /// it into system memory). The advisor keeps the model on Ollama from here on.
    fn declined(&self, _model_id: &str, _why: &str) {}
}

/// Hears about the engine's work, so a client shows it. The daemon implements it.
pub trait EngineObserver: Send + Sync {
    /// A load begins; the returned guard is dropped when it ends, loaded or not.
    fn loading(&self, model: &str, detail: &str) -> Box<dyn Send>;
    /// One line about something the engine did on its own (an idle unload).
    fn note(&self, line: &str);
}

#[derive(Clone, Default)]
struct Hooks {
    advisor: Arc<RwLock<Option<Arc<dyn EngineAdvisor>>>>,
    observer: Arc<RwLock<Option<Arc<dyn EngineObserver>>>>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooks")
    }
}

/// Model id → allocated context (tokens). Cheap to clone; every clone sees every update.
#[derive(Debug, Clone, Default)]
pub struct ContextAllocations {
    inner: Arc<RwLock<HashMap<String, u32>>>,
    hooks: Hooks,
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

    /// Installs the advisor every clone consults (the library, once it is shared).
    pub fn set_engine_advisor(&self, advisor: Arc<dyn EngineAdvisor>) {
        *self.hooks.advisor.write().unwrap_or_else(|e| e.into_inner()) = Some(advisor);
    }

    /// How Marlowe's own llama-server should serve `model_id`; `None` leaves it to Ollama (also
    /// while no advisor is installed).
    pub fn engine_shape(&self, model_id: &str) -> Option<EngineShape> {
        let advisor = self.hooks.advisor.read().unwrap_or_else(|e| e.into_inner()).clone()?;
        advisor.shape(model_id)
    }

    /// The GPU memory the planner expects the engine's server for `model_id` to take, MiB.
    pub fn engine_need_mib(&self, model_id: &str) -> Option<f64> {
        let advisor = self.hooks.advisor.read().unwrap_or_else(|e| e.into_inner()).clone()?;
        advisor.need_mib(model_id)
    }

    /// Tells the advisor the engine did not run `model_id`, and why.
    pub fn engine_declined(&self, model_id: &str, why: &str) {
        let advisor = self.hooks.advisor.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(a) = advisor {
            a.declined(model_id, why);
        }
    }

    /// Installs who hears about the engine's loads and unloads.
    pub fn set_engine_observer(&self, observer: Arc<dyn EngineObserver>) {
        *self.hooks.observer.write().unwrap_or_else(|e| e.into_inner()) = Some(observer);
    }

    pub fn engine_observer(&self) -> Option<Arc<dyn EngineObserver>> {
        self.hooks.observer.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advisor_is_shared_by_every_clone_and_absent_means_ollama() {
        struct Six;
        impl EngineAdvisor for Six {
            fn shape(&self, id: &str) -> Option<EngineShape> {
                (id == "m").then(|| EngineShape { slots: 6, ctx_per_slot: 49_152, kv: "q8_0".into() })
            }
        }
        let a = ContextAllocations::new();
        let b = a.clone();
        assert_eq!(b.engine_shape("m"), None);
        a.set_engine_advisor(Arc::new(Six));
        assert_eq!(b.engine_shape("m").map(|s| s.slots), Some(6));
        assert_eq!(b.engine_shape("other"), None);
    }

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
