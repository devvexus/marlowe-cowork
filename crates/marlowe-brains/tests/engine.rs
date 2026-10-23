//! Which models Marlowe's llama.cpp engine serves, and the slots and window the planner gives
//! them, over fakes of the hardware.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_brains::{Brains, Hardware, Library, MemoryStore, ProviderFactory};
use marlowe_hw::{
    GpuBudget, GpuSnapshot, GpuSource, ModelFiles, ModelHeader, ModelSource, RunnerObservation, ServerChange, ServerControl,
    SlotObservation,
};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, ModelInfo, ModelRef, Provider, ProviderError, ProviderKind, StreamEvent,
};
use marlowe_proto::{ChangeWhen, ContextPolicy, Engine, KvType, PolicySource, Slot};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-hw/tests/fixtures")
}

fn header(name: &str) -> ModelHeader {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixtures().join(name)).unwrap()).unwrap();
    ModelHeader::from_show(&v).unwrap()
}

/// `plain:8b` is a model Ollama hands to llama-server whole; `rendered:8b` has a renderer.
struct Models {
    serial: bool,
    running: Mutex<Option<(u16, u32)>>,
}

impl Models {
    fn new() -> Arc<Self> {
        Arc::new(Self { serial: false, running: Mutex::new(None) })
    }
}

impl ModelSource for Models {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        match model_id {
            "plain:8b" | "rendered:8b" => Ok(ModelFiles { main: header("gemma3_12b_q4_k_m.json"), source: "GGUF header".into(), ..Default::default() }),
            other => Err(format!("no model {other}")),
        }
    }
    fn runtime(&self, model_id: &str, _: Option<&str>) -> RunnerObservation {
        let mut rt = RunnerObservation::default();
        if self.serial {
            rt.serial_archs = vec!["llama".into()];
        }
        if let (Some((slots, ctx)), "plain:8b") = (*self.running.lock().unwrap(), model_id) {
            rt.loaded = true;
            rt.ps_context = Some(ctx);
            rt.slots = (0..slots).map(|id| SlotObservation { id: u32::from(id), n_ctx: ctx, n_prompt_tokens: 0, is_processing: false }).collect();
        }
        rt
    }
    fn engine_eligibility(&self, model_id: &str) -> Result<(), String> {
        match model_id {
            "plain:8b" => Ok(()),
            _ => Err("Ollama renders its prompt itself (renderer qwen3.5)".into()),
        }
    }
}

struct Gpu(f64);
impl GpuSource for Gpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        Some(GpuSnapshot {
            gpu: GpuBudget {
                name: "NVIDIA GeForce RTX 4080 SUPER".into(),
                total_mib: 24_576.0,
                used_mib: 24_576.0 - self.0,
                free_mib: self.0,
                runners_mib: 0.0,
                runners_shared_mib: 0.0,
                headroom_mib: 1_024.0,
                bandwidth_gbs: Some(736.0),
            },
            runners: vec![],
            ram_total_mib: Some(31_914.0),
            ram_free_mib: Some(20_000.0),
        })
    }
}

struct Server;
impl ServerControl for Server {
    fn apply(&self, _: &ServerChange) -> Result<String, String> {
        Ok("recorded".into())
    }
}

struct NoChat;
impl Provider for NoChat {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut plain = ModelInfo::bare(ProviderKind::Ollama, "plain:8b");
        plain.context_window = Some(8_192);
        let mut rendered = ModelInfo::bare(ProviderKind::Ollama, "rendered:8b");
        rendered.context_window = Some(8_192);
        Ok(vec![plain, rendered])
    }
    fn stream(&self, _: &ChatRequest, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        Err(ProviderError::unsupported("no chat in this test"))
    }
}

fn brains(models: Arc<Models>, free_mib: f64) -> Arc<Brains> {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models, gpu: Arc::new(Gpu(free_mib)), server: Arc::new(Server) });
    let b = Arc::new(Brains::new(Library::open_in_memory().unwrap(), f));
    b.add_model(ProviderKind::Ollama, "plain:8b").unwrap();
    b.add_model(ProviderKind::Ollama, "rendered:8b").unwrap();
    b.install_engine_advisor();
    b
}

fn plain() -> ModelRef {
    "ollama/plain:8b".parse().unwrap()
}

fn rendered() -> ModelRef {
    "ollama/rendered:8b".parse().unwrap()
}

fn user(max_ctx: u32, parallel: u16) -> ContextPolicy {
    ContextPolicy { max_ctx, kv_type: KvType::Q8_0, parallel, source: PolicySource::User, updated_ms: 0 }
}

#[test]
fn an_eligible_model_runs_on_the_engine_and_ollamas_models_stay_on_ollama() {
    let b = brains(Models::new(), 20_000.0);
    assert!(b.engine_wanted(&plain()));
    assert!(!b.engine_wanted(&rendered()));
    let kept = b.context_plan(&rendered()).unwrap();
    assert_eq!(kept.in_effect.engine, Engine::Ollama);
    assert_eq!(kept.in_effect.engine_note.as_deref(), Some("Ollama keeps this model: Ollama renders its prompt itself (renderer qwen3.5)"));
    assert_eq!(b.context_plan(&plain()).unwrap().in_effect.engine, Engine::LlamaCpp);
}

#[test]
fn with_room_the_planner_starts_at_six_slots_and_says_what_each_agent_gets() {
    let b = brains(Models::new(), 20_000.0);
    let plan = b.context_plan(&plain()).unwrap();
    let auto = plan.auto.expect("an automatic choice");
    assert_eq!(auto.parallel, 6, "the engine starts at six slots when each still gets a useful window");
    assert!(auto.max_ctx >= 16_384, "{auto:?}");
    assert_eq!(auto.kv_type, KvType::Q8_0);
    assert_eq!(plan.policy, auto, "nothing saved: the automatic choice is the policy");
    assert!(plan.warnings.iter().any(|w| w.starts_with("llama.cpp serves 6 slots of ")), "{:?}", plan.warnings);
    // The conductor keeps one slot of the six; the crew runs on five.
    assert_eq!(b.resolve(&Slot::Conductor).unwrap().0, plain());
    assert_eq!(b.context().slots(&plain()).unwrap(), 6);
    assert_eq!(b.context().crew_slots(&plain()), 5);
}

#[test]
fn the_provider_is_told_the_shape_through_the_advisor() {
    let b = brains(Models::new(), 20_000.0);
    let shape = b.factory().allocations().engine_shape("plain:8b").expect("the engine serves it");
    assert_eq!((shape.slots, shape.kv.as_str()), (6, "q8_0"));
    assert_eq!(shape.ctx_per_slot, b.context().policy(&plain()).unwrap().max_ctx);
    assert!(b.factory().allocations().engine_shape("rendered:8b").is_none());
    assert!(b.factory().allocations().engine_shape("unknown:1").is_none());
}

#[test]
fn a_tight_gpu_gives_fewer_agents_a_usable_window_each() {
    let b = brains(Models::new(), 12_500.0);
    let auto = b.context_plan(&plain()).unwrap().auto.expect("an automatic choice");
    assert!(auto.parallel < 6, "{auto:?}");
    assert!(auto.max_ctx >= 16_384 || auto.parallel == 1, "{auto:?}");
}

#[test]
fn a_saved_policy_is_the_launch() {
    let b = brains(Models::new(), 20_000.0);
    b.context().set_policy(&plain(), user(32_768, 3)).unwrap();
    let shape = b.factory().allocations().engine_shape("plain:8b").unwrap();
    assert_eq!((shape.slots, shape.ctx_per_slot), (3, 32_768));
    assert_eq!(b.context().crew_slots(&plain()), 2);
}

#[test]
fn saving_a_policy_for_an_engine_model_restarts_no_ollama() {
    let b = brains(Models::new(), 20_000.0);
    let changes = b.context().set_policy(&plain(), user(24_576, 4)).unwrap();
    assert!(changes.iter().all(|c| c.when == ChangeWhen::Now), "{changes:?}");
}

#[test]
fn turning_the_engine_off_hands_the_model_back_to_ollama() {
    let b = brains(Models::new(), 20_000.0);
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some());
    b.set_model_engine(&plain(), false).unwrap();
    assert!(b.factory().allocations().engine_shape("plain:8b").is_none());
    let plan = b.context_plan(&plain()).unwrap();
    assert_eq!(plan.in_effect.engine, Engine::Ollama);
    assert_eq!(plan.in_effect.engine_note.as_deref(), Some("the llama.cpp engine is turned off for this model"));
    b.set_model_engine(&plain(), true).unwrap();
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some());
}

#[test]
fn the_engines_models_are_not_held_to_ollamas_one_request_at_a_time() {
    let models = Arc::new(Models { serial: true, running: Mutex::new(None) });
    let b = brains(models, 20_000.0);
    // Ollama would serve this architecture one request at a time; the engine does not.
    assert_eq!(b.context().slots(&plain()).unwrap(), 6);
    assert_eq!(b.context().slots_note(&plain()), None);
    // The model Ollama keeps is still held to what Ollama serves.
    b.context().set_policy(&rendered(), user(8_192, 6)).unwrap();
}

#[test]
fn what_the_engine_really_runs_is_what_the_crew_may_use() {
    let models = Models::new();
    let b = brains(models.clone(), 20_000.0);
    b.context().set_policy(&plain(), user(16_384, 6)).unwrap();
    *models.running.lock().unwrap() = Some((4, 16_384));
    // Loaded with four slots (an older shape): the crew uses what is served, less the conductor's.
    // The cached look at the runner is refreshed when it ages; ask for a fresh one.
    let _ = b.context_plan(&plain()).unwrap();
    assert_eq!(b.context().slots(&plain()).unwrap(), 4);
    assert_eq!(b.context().crew_slots(&plain()), 3);
}

#[test]
fn a_model_with_no_engine_in_the_build_is_never_assigned() {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    let b = Arc::new(Brains::new(Library::open_in_memory().unwrap(), f));
    b.add_model(ProviderKind::Ollama, "plain:8b").unwrap();
    b.install_engine_advisor();
    assert!(b.factory().allocations().engine_shape("plain:8b").is_none());
}
