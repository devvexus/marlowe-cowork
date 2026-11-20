//! A close unloads every model Marlowe's own server runs, the library's or not (a Hugging Face or
//! folder file has no library row); a runner that is someone else's stays.

use std::sync::{Arc, Mutex};

use marlowe_brains::{Brains, Hardware, Library, MemoryStore, ProviderFactory};
use marlowe_hw::{GpuSnapshot, GpuSource, ModelFiles, ModelSource, RunnerObservation, ServerChange, ServerControl};
use marlowe_model::{CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StreamEvent};

struct NoChat;
impl Provider for NoChat {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
    fn stream(&self, _: &ChatRequest, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        Err(ProviderError::unsupported("no chat in this test"))
    }
}

struct Models {
    loaded: Vec<String>,
    own: bool,
    unloaded: Mutex<Vec<String>>,
}

impl ModelSource for Models {
    fn files(&self, id: &str) -> Result<ModelFiles, String> {
        Err(format!("no model {id}"))
    }
    fn runtime(&self, _: &str, _: Option<&str>) -> RunnerObservation {
        RunnerObservation::default()
    }
    fn loaded_models(&self) -> Vec<String> {
        self.loaded.clone()
    }
    fn unload(&self, id: &str) -> Result<(), String> {
        self.unloaded.lock().unwrap().push(id.to_string());
        Ok(())
    }
    fn runs_on_own_server(&self, _: &str) -> bool {
        self.own
    }
}

struct NoGpu;
impl GpuSource for NoGpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        None
    }
}

struct NoServer;
impl ServerControl for NoServer {
    fn apply(&self, _: &ServerChange) -> Result<String, String> {
        Ok(String::new())
    }
}

fn close_with(models: Arc<Models>) -> Vec<String> {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: models.clone(), gpu: Arc::new(NoGpu), server: Arc::new(NoServer) });
    let b = Brains::new(Library::open_in_memory().unwrap(), f);
    b.add_model(ProviderKind::Ollama, "dusk:27b").unwrap();
    let (done, failed) = b.unload_local_models_observed(false, &mut |_, _| {});
    assert!(failed.is_empty());
    let mut got = models.unloaded.lock().unwrap().clone();
    got.sort();
    let mut done = done;
    done.sort();
    assert_eq!(got, done);
    got
}

#[test]
fn a_close_stops_the_own_server_whatever_model_it_runs() {
    let m = Arc::new(Models { loaded: vec!["gguf/hf/Qwen3.8-27B-MS2.7.gguf".into()], own: true, unloaded: Mutex::default() });
    assert_eq!(close_with(m), vec!["gguf/hf/Qwen3.8-27B-MS2.7.gguf".to_string()]);
}

#[test]
fn a_model_in_the_library_is_unloaded_and_one_that_is_someone_elses_stays() {
    let m = Arc::new(Models { loaded: vec!["dusk:27b".into(), "other:1b".into()], own: false, unloaded: Mutex::default() });
    assert_eq!(close_with(m), vec!["dusk:27b".to_string()]);
}
