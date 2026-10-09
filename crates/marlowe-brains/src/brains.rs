//! The facade the daemon delegates library ops to.

use std::path::Path;
use std::sync::{Arc, Mutex};

use marlowe_model::{Effort, ModelInfo, ModelRef, Provider, ProviderKind};
use marlowe_proto::{Event, LibraryModel, ProviderStatus, Slot, SlotAssignment};

use crate::detect::detect_providers;
use crate::error::BrainsError;
use crate::factory::ProviderFactory;
use crate::library::Library;

/// Library + provider factory. `Send + Sync`; share as `Arc<Brains>`.
pub struct Brains {
    library: Mutex<Library>,
    factory: ProviderFactory,
}

impl Brains {
    pub fn new(library: Library, factory: ProviderFactory) -> Self {
        Self { library: Mutex::new(library), factory }
    }

    /// Opens the library in `cowork_db`.
    pub fn open(cowork_db: &Path, factory: ProviderFactory) -> Result<Self, BrainsError> {
        Ok(Self::new(Library::open(cowork_db)?, factory))
    }

    pub fn factory(&self) -> &ProviderFactory {
        &self.factory
    }

    /// Runs `f` with the library locked.
    pub fn with_library<T>(&self, f: impl FnOnce(&Library) -> T) -> T {
        f(&self.library.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The `LibraryState` event: models, resolved slots, provider statuses.
    pub fn state(&self) -> Result<Event, BrainsError> {
        let (models, slots) = self.with_library(|l| Ok::<_, BrainsError>((l.models()?, l.slots()?)))?;
        Ok(Event::LibraryState { models, slots, providers: self.detect() })
    }

    /// Provider statuses (detects Ollama and keys).
    pub fn detect(&self) -> Vec<ProviderStatus> {
        detect_providers(self.factory.credentials().as_ref())
    }

    /// The provider's catalog. `Unsupported` for providers not available now.
    pub fn catalog(&self, kind: ProviderKind) -> Result<Vec<ModelInfo>, BrainsError> {
        self.factory.provider(kind)?.list_models().map_err(Into::into)
    }

    /// Adds `model_id` using its catalog info (bare info if the catalog lacks it).
    pub fn add_model(&self, kind: ProviderKind, model_id: &str) -> Result<LibraryModel, BrainsError> {
        let info = self
            .catalog(kind)?
            .into_iter()
            .find(|m| m.id == model_id)
            .unwrap_or_else(|| ModelInfo::bare(kind, model_id));
        let model = self.with_library(|l| l.add(&info))?;
        self.with_library(|l| l.get(&model))?.ok_or(BrainsError::UnknownModel(model))
    }

    pub fn remove_model(&self, model: &ModelRef) -> Result<(), BrainsError> {
        self.with_library(|l| l.remove(model))?;
        Ok(())
    }

    pub fn set_slot(&self, slot: &Slot, model: Option<&ModelRef>, effort: Effort) -> Result<(), BrainsError> {
        self.with_library(|l| l.set_slot(slot, model, effort))
    }

    /// Resolved slot table.
    pub fn slots(&self) -> Result<Vec<SlotAssignment>, BrainsError> {
        self.with_library(|l| l.slots())
    }

    /// Stores a key in the credential store and rebuilds that provider on next use.
    pub fn set_credential(&self, kind: ProviderKind, secret: &str) -> Result<(), BrainsError> {
        if secret.trim().is_empty() {
            return Err(BrainsError::Invalid("empty credential".into()));
        }
        self.factory.credentials().set(kind, secret.trim())?;
        self.factory.invalidate(kind);
        Ok(())
    }

    /// The model and effort for `slot`. `NoModels` when the library is empty.
    pub fn resolve(&self, slot: &Slot) -> Result<(ModelRef, Effort), BrainsError> {
        self.with_library(|l| l.resolve(slot))
    }

    /// Stored info for a library model.
    pub fn model_info(&self, model: &ModelRef) -> Result<ModelInfo, BrainsError> {
        self.with_library(|l| l.get(model))?
            .map(|m| m.info)
            .ok_or_else(|| BrainsError::UnknownModel(model.clone()))
    }

    /// The provider serving `model`.
    pub fn provider_for(&self, model: &ModelRef) -> Result<Arc<dyn Provider>, BrainsError> {
        self.factory.provider(model.provider)
    }

    /// Everything an agent needs for `slot`: provider, model info, effort.
    pub fn agent_parts(&self, slot: &Slot) -> Result<(Arc<dyn Provider>, ModelInfo, Effort), BrainsError> {
        let (model, effort) = self.resolve(slot)?;
        Ok((self.provider_for(&model)?, self.model_info(&model)?, effort))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use marlowe_model::{CancelToken, ChatRequest, Completion, ProviderError, StreamEvent};

    struct Fake;
    impl Provider for Fake {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Ollama
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            let mut m = ModelInfo::bare(ProviderKind::Ollama, "qwen3:8b");
            m.display = "Qwen3 8B".into();
            m.supports_tools = true;
            Ok(vec![m])
        }
        fn stream(
            &self,
            _: &ChatRequest,
            _: &CancelToken,
            _: &mut dyn FnMut(StreamEvent),
        ) -> Result<Completion, ProviderError> {
            Err(ProviderError::unsupported("fake"))
        }
    }

    fn brains() -> Brains {
        let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
        f.register(ProviderKind::Ollama, |_| Ok(Arc::new(Fake)));
        f.register(ProviderKind::OpenRouter, |_| Ok(Arc::new(Fake)));
        Brains::new(Library::open_in_memory().unwrap(), f)
    }

    #[test]
    fn onboarding_flow() {
        let b = brains();
        assert!(matches!(b.resolve(&Slot::Conductor), Err(BrainsError::NoModels)));
        assert!(matches!(b.catalog(ProviderKind::Anthropic), Err(BrainsError::Unsupported(_))));
        let added = b.add_model(ProviderKind::Ollama, "qwen3:8b").unwrap();
        assert_eq!(added.info.display, "Qwen3 8B");
        let bare = b.add_model(ProviderKind::Ollama, "other").unwrap();
        let (m, _) = b.resolve(&Slot::Conductor).unwrap();
        assert_eq!(m, added.model);
        b.set_slot(&Slot::Reflection, Some(&bare.model), Effort::Low).unwrap();
        let (_, info, effort) = b.agent_parts(&Slot::Reflection).unwrap();
        assert_eq!((info.id.as_str(), effort), ("other", Effort::Low));
        match b.state().unwrap() {
            Event::LibraryState { models, slots, providers } => {
                assert_eq!((models.len(), slots.len(), providers.len()), (2, 3, 5));
            }
            e => panic!("{e:?}"),
        }
        b.remove_model(&bare.model).unwrap();
        assert_eq!(b.resolve(&Slot::Reflection).unwrap().0, added.model);
    }

    #[test]
    fn credentials_are_stored_and_trimmed() {
        let b = brains();
        assert!(b.set_credential(ProviderKind::OpenRouter, "  ").is_err());
        b.set_credential(ProviderKind::OpenRouter, " sk-x 
").unwrap();
        let got = b.factory().credentials().get(ProviderKind::OpenRouter).unwrap();
        assert_eq!(got.as_deref(), Some("sk-x"));
    }
}
