//! One provider in front of several: a project whose employees and lead run on different models
//! (`projects.model.employee`, `.lead`) calls them through the provider that serves each.

use std::collections::HashMap;
use std::sync::Arc;

use marlowe_model::{CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StreamEvent};

/// Sends each request to the provider registered for its model id, else to the default.
pub struct ByModel {
    default: Arc<dyn Provider>,
    by_model: HashMap<String, Arc<dyn Provider>>,
}

impl ByModel {
    /// Everything goes to `default` until a model is [`ByModel::route`]d elsewhere.
    pub fn new(default: Arc<dyn Provider>) -> ByModel {
        ByModel { default, by_model: HashMap::new() }
    }

    /// Requests for `model` go to `provider`.
    pub fn route(mut self, model: &str, provider: Arc<dyn Provider>) -> ByModel {
        self.by_model.insert(model.to_string(), provider);
        self
    }

    fn pick(&self, model: &str) -> &Arc<dyn Provider> {
        self.by_model.get(model).unwrap_or(&self.default)
    }
}

impl Provider for ByModel {
    fn kind(&self) -> ProviderKind {
        self.default.kind()
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.default.list_models()
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.pick(&req.model).stream(req, cancel, on)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use marlowe_model::{CompletionBuilder, Message, StopReason};

    use super::*;

    struct Named(&'static str, Mutex<Vec<String>>);

    impl Provider for Named {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Ollama
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
        fn stream(&self, req: &ChatRequest, _c: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
            self.1.lock().unwrap().push(req.model.clone());
            let mut b = CompletionBuilder::new();
            for e in [StreamEvent::TextDelta(self.0.into()), StreamEvent::Stop(StopReason::EndTurn)] {
                b.push(&e);
                on(e);
            }
            Ok(b.finish())
        }
    }

    #[test]
    fn a_request_goes_to_the_provider_of_its_model_and_anything_else_to_the_default() {
        let (conversation, small) = (Arc::new(Named("big", Mutex::default())), Arc::new(Named("small", Mutex::default())));
        let route = ByModel::new(conversation.clone()).route("mini:4b", small.clone());
        let ask = |model: &str| route.stream(&ChatRequest::new(model, vec![Message::user("hi")]), &CancelToken::new(), &mut |_| {}).unwrap().text;
        assert_eq!((ask("mini:4b"), ask("dawn:9b"), ask("other")), ("small".to_string(), "big".to_string(), "big".to_string()));
        assert_eq!(small.1.lock().unwrap().as_slice(), ["mini:4b"]);
        assert_eq!(conversation.1.lock().unwrap().len(), 2);
    }
}
