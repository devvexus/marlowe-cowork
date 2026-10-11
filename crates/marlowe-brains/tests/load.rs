//! Load admission over the ui-preview5 library that loaded valiance in the background: valiance
//! added first (so it is the library's first model), dawn second, the conductor and quick on
//! dawn, reflection and crew:general unset, dawn loaded on a 16 GiB card.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_brains::{Admission, Brains, Hardware, Library, MemoryStore, Priority, ProviderFactory};
use marlowe_hw::{
    GpuBudget, GpuSnapshot, GpuSource, ModelFiles, ModelHeader, ModelSource, RunnerObservation, ServerChange, ServerControl,
};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, Effort, ModelInfo, ModelRef, Provider, ProviderError, ProviderKind, StreamEvent,
};
use marlowe_proto::Slot;

const VALIANCE: &str = "marlowe-valiance:22b";
const DAWN: &str = "marlowe-dawn:9b";
/// A model whose files cannot be read: the planner has no cost for it.
const MYSTERY: &str = "mystery:7b";

fn header(name: &str) -> ModelHeader {
    let dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-hw/tests/fixtures");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap();
    ModelHeader::from_show(&v).unwrap()
}

/// Valiance stands in as the 27B dusk-super header (with its draft), dawn as a llama3 8B.
#[derive(Default)]
struct Engine {
    loaded: Mutex<Vec<String>>,
    /// `/api/ps` does not answer.
    ps_down: std::sync::atomic::AtomicBool,
}

impl ModelSource for Engine {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        match model_id {
            VALIANCE => Ok(ModelFiles {
                main: header("qwen35_dusk_27b_super.json"),
                draft: Some(header("qwen35_dusk_draft_mtp.json")),
                source: "GGUF header".into(),
                ..Default::default()
            }),
            DAWN => Ok(ModelFiles { main: header("llama3_8b_q4_0.json"), source: "GGUF header".into(), ..Default::default() }),
            other => Err(format!("no model {other}")),
        }
    }
    fn runtime(&self, model_id: &str, _: Option<&str>) -> RunnerObservation {
        RunnerObservation { loaded: self.loaded.lock().unwrap().iter().any(|m| m == model_id), ..Default::default() }
    }
    fn loaded_models(&self) -> Vec<String> {
        self.loaded.lock().unwrap().clone()
    }
    fn resident(&self) -> Option<Vec<String>> {
        (!self.ps_down.load(std::sync::atomic::Ordering::SeqCst)).then(|| self.loaded_models())
    }
}

/// A 16 GiB card: the desktop's 1 GiB, plus 6 GiB while dawn's runner is on it.
struct Gpu(Arc<Engine>);
impl GpuSource for Gpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        if self.0.loaded.lock().unwrap().is_empty() {
            let mut s = self.snapshot_with_dawn()?;
            s.gpu.used_mib = 1_000.0;
            s.gpu.free_mib = 15_376.0;
            s.gpu.runners_mib = 0.0;
            s.runners.clear();
            return Some(s);
        }
        self.snapshot_with_dawn()
    }
}

impl Gpu {
    fn snapshot_with_dawn(&self) -> Option<GpuSnapshot> {
        Some(GpuSnapshot {
            gpu: GpuBudget {
                name: "NVIDIA GeForce RTX 4080 SUPER".into(),
                total_mib: 16_376.0,
                used_mib: 7_000.0,
                free_mib: 9_376.0,
                runners_mib: 6_000.0,
                runners_shared_mib: 0.0,
                headroom_mib: 1_024.0,
                bandwidth_gbs: Some(736.0),
            },
            runners: vec![(4242, 6_000.0)],
            ram_total_mib: Some(31_914.0),
            ram_free_mib: Some(12_000.0),
        })
    }
}

struct NoServer;
impl ServerControl for NoServer {
    fn apply(&self, _: &ServerChange) -> Result<String, String> {
        Err("not in this test".into())
    }
}

struct NoChat;
impl Provider for NoChat {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok([VALIANCE, DAWN, MYSTERY]
            .into_iter()
            .map(|id| {
                let mut m = ModelInfo::bare(ProviderKind::Ollama, id);
                m.context_window = Some(32_768);
                m
            })
            .collect())
    }
    fn stream(&self, _: &ChatRequest, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        Err(ProviderError::unsupported("no chat in this test"))
    }
}

fn model(id: &str) -> ModelRef {
    format!("ollama/{id}").parse().unwrap()
}

/// The preview's library and slots, exactly.
fn preview(engine: Arc<Engine>) -> Brains {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: engine.clone(), gpu: Arc::new(Gpu(engine)), server: Arc::new(NoServer) });
    let b = Brains::new(Library::open_in_memory().unwrap(), f);
    b.add_model(ProviderKind::Ollama, VALIANCE).unwrap();
    b.add_model(ProviderKind::Ollama, DAWN).unwrap();
    b.set_slot(&Slot::Conductor, Some(&model(DAWN)), Effort::Medium).unwrap();
    b.set_slot(&Slot::Quick, Some(&model(DAWN)), Effort::Low).unwrap();
    b.set_slot(&Slot::Reflection, None, Effort::Low).unwrap();
    b.set_slot(&Slot::Crew("general".into()), None, Effort::Medium).unwrap();
    b
}

fn background_slots() -> [Slot; 3] {
    [Slot::Reflection, Slot::Quick, Slot::Crew("general".into())]
}

#[test]
fn no_background_path_can_load_valiance_in_the_preview_library() {
    let engine = Arc::new(Engine { loaded: Mutex::new(vec![DAWN.into()]), ..Default::default() });
    let b = preview(engine.clone());
    assert_eq!(b.with_library(|l| l.default_model()).unwrap(), Some(model(VALIANCE)), "the trap: valiance was added first");

    // Unset slots run the conductor's model, never the first one added.
    for slot in background_slots() {
        assert_eq!(b.resolve(&slot).unwrap().0, model(DAWN), "{slot}");
        assert_eq!(b.background_model(&slot).unwrap().0, model(DAWN), "{slot}");
    }
    assert!(b.slots().unwrap().iter().all(|a| a.model == Some(model(DAWN))));

    // Valiance itself: background never, a crew run not beside dawn, the user's own turn asks.
    assert!(matches!(b.admit(&model(VALIANCE), Priority::Background), Admission::Refused(_)));
    match b.admit(&model(VALIANCE), Priority::Crew) {
        Admission::Refused(why) => assert!(why.contains("needs") && why.contains("beside marlowe-dawn:9b"), "{why}"),
        other => panic!("{other:?}"),
    }
    match b.admit(&model(VALIANCE), Priority::Foreground) {
        Admission::Ask(cost) => {
            assert_eq!(cost.fits(), Some(false));
            assert_eq!(cost.resident, vec![DAWN.to_string()]);
            assert!(cost.need_mib.unwrap() > cost.free_mib.unwrap(), "{cost:?}");
            assert_eq!(cost.free_mib, Some(9_376.0 - 1_024.0));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(b.admit(&model(DAWN), Priority::Background), Admission::Resident);

    // With nothing loaded, background work waits: it does not cold-load even the conductor's model.
    engine.loaded.lock().unwrap().clear();
    for slot in background_slots() {
        let err = b.background_model(&slot).unwrap_err().to_string();
        assert!(err.contains("never loads a model"), "{slot}: {err}");
    }
    // Even an explicit reflection assignment to valiance cannot pull it in behind the user's back.
    b.set_slot(&Slot::Reflection, Some(&model(VALIANCE)), Effort::Low).unwrap();
    engine.loaded.lock().unwrap().push(DAWN.into());
    assert_eq!(b.background_model(&Slot::Reflection).unwrap().0, model(DAWN), "falls back to the resident conductor");
}

#[test]
fn a_cold_load_that_fits_is_admitted_with_its_cost() {
    let engine = Arc::new(Engine::default());
    let b = preview(engine);
    match b.admit(&model(DAWN), Priority::Foreground) {
        Admission::Load(cost) => {
            assert_eq!(cost.fits(), Some(true), "{cost:?}");
            assert!(cost.resident.is_empty());
            assert_eq!(cost.ctx, 32_768);
        }
        other => panic!("{other:?}"),
    }
    assert!(b.admit(&model(DAWN), Priority::Crew).may_run());
    assert!(!b.admit(&model(DAWN), Priority::Background).may_run());
}

/// Unknown is not "fits": a model the planner cannot cost asks on the user's turn and is refused
/// to a crew step; it never cold-loads unasked.
#[test]
fn a_load_with_no_measured_cost_asks_or_is_refused() {
    let engine = Arc::new(Engine { loaded: Mutex::new(vec![DAWN.into()]), ..Default::default() });
    let b = preview(engine);
    b.add_model(ProviderKind::Ollama, MYSTERY).unwrap();
    match b.admit(&model(MYSTERY), Priority::Foreground) {
        Admission::Ask(cost) => {
            assert_eq!(cost.need_mib, None);
            assert!(cost.describe().contains("has no measured cost"), "{}", cost.describe());
        }
        other => panic!("{other:?}"),
    }
    match b.admit(&model(MYSTERY), Priority::Crew) {
        Admission::Refused(why) => assert!(why.contains("has no measured cost"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(b.admit(&model(MYSTERY), Priority::Background), Admission::Refused(_)));
}

/// A failed `/api/ps` means what is loaded is unknown, not that nothing is: a resident model is
/// not costed as a cold load. The user's turn asks, saying so; a crew step waits; background work
/// does not run.
#[test]
fn an_engine_that_does_not_say_what_is_loaded_is_not_taken_as_empty() {
    let engine = Arc::new(Engine { loaded: Mutex::new(vec![DAWN.into()]), ..Default::default() });
    let b = preview(engine.clone());
    engine.ps_down.store(true, std::sync::atomic::Ordering::SeqCst);
    match b.admit(&model(DAWN), Priority::Foreground) {
        Admission::Ask(cost) => {
            assert!(!cost.resident_known);
            assert_eq!(cost.fits(), None);
            assert!(cost.describe().contains("did not say what is loaded"), "{}", cost.describe());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(b.admit(&model(DAWN), Priority::Crew), Admission::Wait(_)));
    assert!(matches!(b.admit(&model(DAWN), Priority::Background), Admission::Refused(_)));
    assert!(b.background_model(&Slot::Quick).is_err());
    engine.ps_down.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(b.admit(&model(DAWN), Priority::Crew), Admission::Resident);
}
