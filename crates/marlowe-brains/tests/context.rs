//! The context gate and the planner over fakes: the real dusk-super headers and runner log, a
//! GPU snapshot as PDH measured it, and a server control that only records what it was asked.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_brains::{Brains, Hardware, Library, MemoryStore, ProviderFactory};
use marlowe_hw::log;
use marlowe_hw::{
    GpuBudget, GpuSnapshot, GpuSource, ModelFiles, ModelHeader, ModelSource, RunnerObservation, ServerChange, ServerControl,
    ServerEnv, SlotObservation,
};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, ModelInfo, ModelRef, Provider, ProviderError, ProviderKind, StreamEvent,
};
use marlowe_proto::{ChangeWhen, ContextPolicy, Fit, KvType, PolicySource, Slot};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-hw/tests/fixtures")
}

fn header(name: &str) -> ModelHeader {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixtures().join(name)).unwrap()).unwrap();
    ModelHeader::from_show(&v).unwrap()
}

const BLOB: &str = r"C:\Users\matth\.ollama\models\blobs\sha256-17c315b6e2fc165a94c52bc7ace9b8ef8fd599ded3dd5bd7dae5ce17479a4d15";

struct FakeModels;
impl ModelSource for FakeModels {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        match model_id {
            "marlowe-dusk:27b-super" => Ok(ModelFiles {
                main: header("qwen35_dusk_27b_super.json"),
                draft: Some(header("qwen35_dusk_draft_mtp.json")),
                mmproj_bytes: 0,
                blob: Some(BLOB.into()),
                source: "GGUF header".into(),
                quant: Some("IQ3_XXS".into()),
            }),
            "dolphin-llama3:8b" => Ok(ModelFiles { main: header("llama3_8b_q4_0.json"), source: "GGUF header".into(), ..Default::default() }),
            other => Err(format!("no model {other}")),
        }
    }
    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        let blocks = log::parse(&std::fs::read_to_string(fixtures().join("server_dusk_super.log")).unwrap());
        let loaded = model_id == "marlowe-dusk:27b-super";
        RunnerObservation {
            load: blob.and_then(|b| log::last_for_blob(&blocks, b)).cloned(),
            loaded,
            ps_context: loaded.then_some(32_768),
            slots: if loaded { vec![SlotObservation { id: 0, n_ctx: 32_768, n_prompt_tokens: 1187, is_processing: false }] } else { vec![] },
            saved: ServerEnv { kv_cache_type: Some("q8_0".into()), num_parallel: Some("1".into()), flash_attention: Some("1".into()) },
            serial_archs: Vec::new(),
        }
    }
}

struct FakeGpu;
impl GpuSource for FakeGpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        // As PDH and NVML read it with dusk-super loaded: 13,485.7 MiB dedicated to the runner.
        Some(GpuSnapshot {
            gpu: GpuBudget {
                name: "NVIDIA GeForce RTX 4080 SUPER".into(),
                total_mib: 16_376.0,
                used_mib: 14_799.7,
                free_mib: 1_576.3,
                runners_mib: 13_485.7,
                runners_shared_mib: 1_122.0,
                headroom_mib: 1_024.0,
                bandwidth_gbs: Some(736.0),
            },
            runners: vec![(8236, 13_485.7)],
            ram_total_mib: Some(31_914.0),
            ram_free_mib: Some(12_300.0),
        })
    }
}

#[derive(Default)]
struct RecordingServer(Mutex<Vec<ServerChange>>);
impl ServerControl for RecordingServer {
    fn apply(&self, change: &ServerChange) -> Result<String, String> {
        self.0.lock().unwrap().push(*change);
        Ok("recorded".into())
    }
}

struct NoChat;
impl Provider for NoChat {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut dusk = ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super");
        dusk.context_window = Some(32_768);
        let mut llama = ModelInfo::bare(ProviderKind::Ollama, "dolphin-llama3:8b");
        llama.context_window = Some(8_192);
        Ok(vec![dusk, llama])
    }
    fn stream(&self, _: &ChatRequest, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        Err(ProviderError::unsupported("no chat in this test"))
    }
}

fn brains(server: Arc<RecordingServer>) -> Brains {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: Arc::new(FakeModels), gpu: Arc::new(FakeGpu), server });
    let b = Brains::new(Library::open_in_memory().unwrap(), f);
    b.add_model(ProviderKind::Ollama, "marlowe-dusk:27b-super").unwrap();
    b.add_model(ProviderKind::Ollama, "dolphin-llama3:8b").unwrap();
    b
}

fn dusk() -> ModelRef {
    "ollama/marlowe-dusk:27b-super".parse().unwrap()
}

fn user(max_ctx: u32, kv_type: KvType, parallel: u16) -> ContextPolicy {
    ContextPolicy { max_ctx, kv_type, parallel, source: PolicySource::User, updated_ms: 0 }
}

#[test]
fn the_plan_reads_the_runner_and_counts_agents() {
    let b = brains(Arc::default());
    let p = b.context_plan(&dusk()).unwrap();
    let plan = p.plan.as_ref().unwrap();
    assert_eq!(p.arch.summary, "64 layers, 16 keep a KV cache, 48 Gated DeltaNet, 1 MTP");
    assert_eq!(p.arch.state_kind.as_deref(), Some("Gated DeltaNet"));
    assert_eq!(p.ctx_limit, 262_144);
    assert!(p.in_effect.loaded);
    assert_eq!(p.in_effect.kv_type, Some(KvType::Q8_0));
    assert_eq!(p.in_effect.gpu_layers, Some((66, 66)));
    assert_eq!(p.in_effect.spec_n, Some(3));
    assert_eq!(plan.state_copies, 4, "three draft positions: four copies of the state");
    // Calibrated from the load: the driver's share is PDH minus every buffer.
    assert!(plan.calibrated);
    assert!((plan.driver_mib - 506.2).abs() < 0.1, "{}", plan.driver_mib);
    // Budget: free 1,576 + the runner's 13,486 − 1,024 headroom.
    assert!((plan.budget_mib().unwrap() - 14_038.0).abs() < 0.1);
    let row32 = p.rows.iter().find(|r| r.ctx == 32_768).unwrap();
    assert_eq!(row32.agents, [Some(0), Some(1), Some(1)]);
    let row256 = p.rows.iter().find(|r| r.ctx == 262_144).unwrap();
    assert!(matches!(row256.fit[KvType::Q8_0.index()], Fit::NoFit { .. }));
    let auto = p.auto.unwrap();
    assert_eq!((auto.kv_type, auto.parallel), (KvType::Q8_0, 1));
    assert!((40_960..=49_152).contains(&auto.max_ctx), "{}", auto.max_ctx);
    // No saved policy: the window requests use now.
    assert_eq!(p.policy.max_ctx, 32_768);
    assert_eq!(p.policy.source, PolicySource::Auto);
}

#[test]
fn allowed_ctx_is_never_above_max_ctx() {
    let b = brains(Arc::default());
    let gate = b.context();
    let m = dusk();
    // A deterministic sweep: every saved max against many requests, small and huge.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..400 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let max = 2_048 + (x % 260_000) as u32;
        gate.set_policy(&m, user(max, KvType::Q8_0, 1)).unwrap();
        let requested = match i % 4 {
            0 => None,
            1 => Some((x >> 20) as u32),
            2 => Some(u32::MAX),
            _ => Some(max + 1),
        };
        let got = gate.allowed_ctx(&m, requested).unwrap();
        assert!(got <= max, "requested {requested:?} under max {max} got {got}");
        if let Some(r) = requested {
            assert_eq!(got, r.min(max));
        } else {
            assert_eq!(got, max);
        }
    }
}

/// Ollama launched the runner with `-c 65536 -np 2` and split it: two slots of 32k each.
struct SplitSlots;
impl ModelSource for SplitSlots {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        FakeModels.files(model_id)
    }
    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        let mut rt = FakeModels.runtime(model_id, blob);
        if let Some(l) = rt.load.as_mut() {
            l.launch.ctx = Some(65_536);
            l.launch.parallel = Some(2);
        }
        rt.slots = (0..2).map(|id| SlotObservation { id, n_ctx: 32_768, n_prompt_tokens: 0, is_processing: false }).collect();
        rt
    }
}

#[test]
fn a_window_split_across_slots_caps_every_agent_at_the_slot() {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: Arc::new(SplitSlots), gpu: Arc::new(FakeGpu), server: Arc::<RecordingServer>::default() });
    let b = Brains::new(Library::open_in_memory().unwrap(), f);
    b.add_model(ProviderKind::Ollama, "marlowe-dusk:27b-super").unwrap();
    let m = dusk();
    b.context().set_policy(&m, user(65_536, KvType::Q8_0, 2)).unwrap();
    let p = b.context_plan(&m).unwrap();
    assert_eq!(p.in_effect.split_slot_ctx(65_536), Some(32_768));
    assert!(p.warnings.iter().any(|w| w.contains("split num_ctx 65536 across 2 slots")), "{:?}", p.warnings);
    // Past the slot the runner's context shift would drop tokens: the slot is the cap.
    assert_eq!(b.context().allowed_ctx(&m, None).unwrap(), 32_768);
    assert_eq!(b.context().allowed_ctx(&m, Some(16_384)).unwrap(), 16_384);
    let (_, info, _) = b.agent_parts(&Slot::Conductor).unwrap();
    assert_eq!(info.context_window, Some(32_768));
}

#[test]
fn set_policy_refuses_what_the_model_cannot_do_and_reports_changes() {
    let server = Arc::new(RecordingServer::default());
    let b = brains(server.clone());
    let gate = b.context();
    let m = dusk();
    assert!(gate.set_policy(&m, user(300_000, KvType::Q8_0, 1)).is_err(), "above the trained 262,144");
    assert!(gate.set_policy(&m, user(1_024, KvType::Q8_0, 1)).is_err());
    assert!(gate.set_policy(&m, user(32_768, KvType::Q8_0, 0)).is_err());
    assert!(gate.set_policy(&m, user(32_768, KvType::Q8_0, 17)).is_err());
    let changes = gate.set_policy(&m, user(47_104, KvType::Q4_0, 1)).unwrap();
    assert_eq!(changes[0].when, ChangeWhen::Now);
    assert!(changes[0].text.contains("num_ctx 47,104 (was 32,768)"), "{}", changes[0].text);
    assert!(changes.iter().any(|c| c.when == ChangeWhen::Restart && c.text.contains("q8_0 → q4_0")));
    assert!(server.0.lock().unwrap().is_empty(), "saving never touches the server");
    // The provider allocation follows, and the conductor's window is the policy's.
    assert_eq!(b.factory().allocations().get("marlowe-dusk:27b-super"), Some(47_104));
    let (_, info, _) = b.agent_parts(&Slot::Conductor).unwrap();
    assert_eq!(info.context_window, Some(47_104));
    assert_eq!(gate.slots(&m).unwrap(), 1);
    // Removing the model removes its policy and its allocation.
    b.remove_model(&m).unwrap();
    assert_eq!(b.factory().allocations().get("marlowe-dusk:27b-super"), None);
}

#[test]
fn saved_policies_reach_the_provider_at_start() {
    let dir = std::env::temp_dir().join(format!("brains-ctx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("cowork.db");
    {
        let lib = Library::open(&db).unwrap();
        lib.add(&ModelInfo::bare(ProviderKind::Ollama, "dolphin-llama3:8b")).unwrap();
        lib.set_policy(&"ollama/dolphin-llama3:8b".parse().unwrap(), &user(8_192, KvType::F16, 2)).unwrap();
    }
    let f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    let b = Brains::open(&db, f).unwrap();
    assert_eq!(b.factory().allocations().get("dolphin-llama3:8b"), Some(8_192));
    drop(b);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn leases_cap_concurrent_agents_and_release_on_drop() {
    let b = Arc::new(brains(Arc::default()));
    // Not loaded, so the policy's two decide (a loaded runner serving fewer would cap it).
    let m: ModelRef = "ollama/dolphin-llama3:8b".parse().unwrap();
    b.context().set_policy(&m, user(8_192, KvType::Q8_0, 2)).unwrap();
    let one = b.context().lease(&m, Duration::from_millis(10)).unwrap();
    let _two = b.context().lease(&m, Duration::from_millis(10)).unwrap();
    assert!(b.context().lease(&m, Duration::from_millis(30)).is_err(), "a third waits, then gives up");
    let b2 = b.clone();
    let m2 = m.clone();
    let waiter = std::thread::spawn(move || b2.context().lease(&m2, Duration::from_secs(5)).is_ok());
    std::thread::sleep(Duration::from_millis(50));
    drop(one);
    assert!(waiter.join().unwrap(), "a freed slot wakes the waiter");
}

#[test]
fn crew_slots_leave_the_conductor_one() {
    let b = brains(Arc::default());
    let (dusk, dolphin) = (dusk(), "ollama/dolphin-llama3:8b".parse::<ModelRef>().unwrap());
    assert_eq!(b.resolve(&Slot::Conductor).unwrap().0, dusk);
    b.context().set_policy(&dusk, user(16_384, KvType::Q8_0, 6)).unwrap();
    b.context().set_policy(&dolphin, user(8_192, KvType::Q8_0, 6)).unwrap();
    // dusk's loaded runner serves one request at a time (one slot): planning six changes nothing.
    assert_eq!(b.context().crew_slots(&dusk), 1, "what the runner serves, not the plan");
    assert_eq!(b.context().crew_slots(&dolphin), 6, "six slots, the conductor is elsewhere, nothing loaded to say less");
    b.context().set_policy(&dusk, user(16_384, KvType::Q8_0, 1)).unwrap();
    assert_eq!(b.context().crew_slots(&dusk), 1, "never below one");
}

/// Ollama serves six slots: the plan's six stand, one kept for the conductor.
struct SixSlots;
impl ModelSource for SixSlots {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        FakeModels.files(model_id)
    }
    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        let mut rt = FakeModels.runtime(model_id, blob);
        if rt.loaded {
            rt.slots = (0..6).map(|id| SlotObservation { id, n_ctx: 32_768, n_prompt_tokens: 0, is_processing: false }).collect();
        }
        rt
    }
}

/// 2026-10-21: Matthew planned 6 parallel; the running Ollama was started with one slot, so
/// workflow agents never overlapped and nothing said why.
#[test]
fn slots_are_what_the_runner_serves_and_a_shortfall_is_said_plainly() {
    let b = brains(Arc::default());
    let m = dusk();
    b.context().set_policy(&m, user(32_768, KvType::Q8_0, 6)).unwrap();
    assert_eq!(b.context().slots(&m).unwrap(), 1);
    let note = b.context().slots_note(&m).expect("a shortfall");
    assert_eq!(note, "planned 6 parallel, Ollama is serving 1 — restart Ollama to apply");
    let plan = b.context_plan(&m).unwrap();
    assert!(plan.warnings.contains(&note), "the context planner says it too: {:?}", plan.warnings);
    assert_eq!(b.context().slots_note(&"ollama/dolphin-llama3:8b".parse().unwrap()), None, "not loaded: the next load decides");

    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: Arc::new(SixSlots), gpu: Arc::new(FakeGpu), server: Arc::<RecordingServer>::default() });
    let b = Brains::new(Library::open_in_memory().unwrap(), f);
    b.add_model(ProviderKind::Ollama, "marlowe-dusk:27b-super").unwrap();
    b.context().set_policy(&m, user(32_768, KvType::Q8_0, 6)).unwrap();
    assert_eq!((b.context().slots(&m).unwrap(), b.context().crew_slots(&m)), (6, 5));
    assert_eq!(b.context().slots_note(&m), None);
}

/// What Ollama 0.32.5 logs before it launches a qwen35 model (2026-10-21, server.log 18:54):
/// `model architecture does not currently support parallel requests architecture=qwen35`.
struct SerialDusk;
impl ModelSource for SerialDusk {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        FakeModels.files(model_id)
    }
    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        RunnerObservation { serial_archs: vec!["qwen35".into()], ..FakeModels.runtime(model_id, blob) }
    }
}

/// Session 82cb2e7f, 2026-10-21: OLLAMA_NUM_PARALLEL=6, yet Ollama served the qwen35 dusk one
/// request at a time. The conductor's calls queued 5 to 25 s behind a team task's agent under
/// "waiting for the GPU", and the slots note said a restart would apply the 6. The wait and the
/// note now say what is true.
#[test]
fn a_model_ollama_serves_one_at_a_time_says_so_in_its_wait_and_its_note() {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: Arc::new(SerialDusk), gpu: Arc::new(FakeGpu), server: Arc::<RecordingServer>::default() });
    let b = Brains::new(Library::open_in_memory().unwrap(), f);
    b.add_model(ProviderKind::Ollama, "marlowe-dusk:27b-super").unwrap();
    let m = dusk();
    b.context().set_policy(&m, user(32_768, KvType::Q8_0, 6)).unwrap();
    assert_eq!((b.context().slots(&m).unwrap(), b.context().crew_slots(&m)), (1, 1));
    assert_eq!(b.context().slots_note(&m).as_deref(), Some("Ollama runs qwen35 models one request at a time, not the 6 planned"));
    assert_eq!(b.context().queue_reason(&m, 1).as_deref(), Some("Ollama runs qwen35 models one request at a time"));
    assert_eq!(b.context().queue_reason(&m, 0), None, "no crew call in flight: nothing to wait behind");

    // A server started with one slot: a restart applies the plan, and the wait says that.
    let old = brains(Arc::default());
    old.context().set_policy(&m, user(32_768, KvType::Q8_0, 6)).unwrap();
    assert_eq!(old.context().queue_reason(&m, 1).as_deref(), Some("Ollama is serving 1 at a time, not the 6 planned"));

    // Six slots served: two crew calls leave the conductor room; six fill it.
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models: Arc::new(SixSlots), gpu: Arc::new(FakeGpu), server: Arc::<RecordingServer>::default() });
    let six = Brains::new(Library::open_in_memory().unwrap(), f);
    six.add_model(ProviderKind::Ollama, "marlowe-dusk:27b-super").unwrap();
    six.context().set_policy(&m, user(32_768, KvType::Q8_0, 6)).unwrap();
    assert_eq!(six.context().queue_reason(&m, 2), None, "not queued, so no wait line however slow the read");
    assert_eq!(six.context().queue_reason(&m, 6).as_deref(), Some("its 6 slots are all busy"));
}

#[test]
fn policies_list_carries_the_conductors_engine() {
    let b = brains(Arc::default());
    let list = b.context_policies().unwrap();
    assert_eq!(list.len(), 2);
    let dusk = &list[0];
    assert_eq!(dusk.max_ctx, 32_768);
    assert!(dusk.in_effect.loaded);
    assert_eq!(dusk.in_effect.kv_type, Some(KvType::Q8_0));
    assert_eq!(list[1].max_ctx, 8_192);
}

#[test]
fn applying_a_server_change_goes_to_the_control() {
    let server = Arc::new(RecordingServer::default());
    let b = brains(server.clone());
    let change = ServerChange { kv_type: Some(KvType::Q4_0), parallel: None, flash_attn: None };
    assert_eq!(b.apply_server(change).unwrap(), "recorded");
    assert_eq!(server.0.lock().unwrap().as_slice(), &[change]);
}
