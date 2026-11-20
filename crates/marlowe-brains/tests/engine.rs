//! How Marlowe's llama.cpp engine serves every local model (whole on the GPU or split), and the
//! slots and window the planner gives them, over fakes of the hardware.

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
use marlowe_proto::{ChangeWhen, ContextPolicy, Engine, Experts, HostTables, KvType, Placement, PolicySource, Slot};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-hw/tests/fixtures")
}

fn header(name: &str) -> ModelHeader {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixtures().join(name)).unwrap()).unwrap();
    ModelHeader::from_show(&v).unwrap()
}

/// `plain:8b` is a model llama.cpp runs; `rendered:8b` has only Ollama's renderer to go on.
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
            "moe:30b" => Ok(ModelFiles { main: synthetic(48, 64, 0), source: "GGUF header".into(), ..Default::default() }),
            "ngram:4b" => Ok(ModelFiles { main: synthetic(8, 0, 4 * 1024 * 1024 * 1024 / 272), source: "GGUF header".into(), ..Default::default() }),
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
            "plain:8b" | "moe:30b" | "ngram:4b" => Ok(()),
            _ => Err("Ollama renders its prompt with its own code (renderer qwen3.5)".into()),
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

/// A GPU whose free memory the test moves, or that cannot be read at all.
struct Dial(Mutex<Option<f64>>);
impl GpuSource for Dial {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        let free = (*self.0.lock().unwrap())?;
        Gpu(free).snapshot()
    }
}

/// Hears what the engine says on its own.
#[derive(Default)]
struct Ears(Mutex<Vec<String>>);
impl marlowe_model::EngineObserver for Ears {
    fn loading(&self, _: &str, _: &str) -> Box<dyn Send> {
        Box::new(())
    }
    fn note(&self, line: &str) {
        self.0.lock().unwrap().push(line.to_string());
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
        let mut moe = ModelInfo::bare(ProviderKind::Ollama, "moe:30b");
        moe.context_window = Some(32_768);
        let mut ngram = ModelInfo::bare(ProviderKind::Ollama, "ngram:4b");
        ngram.context_window = Some(32_768);
        Ok(vec![plain, rendered, moe, ngram])
    }
    fn stream(&self, _: &ChatRequest, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        Err(ProviderError::unsupported("no chat in this test"))
    }
}

fn brains(models: Arc<Models>, free_mib: f64) -> Arc<Brains> {
    brains_on(models, Arc::new(Gpu(free_mib)))
}

fn brains_on(models: Arc<Models>, gpu: Arc<dyn GpuSource>) -> Arc<Brains> {
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.register(ProviderKind::Ollama, |_| Ok(Arc::new(NoChat)));
    f.set_hardware(Hardware { models, gpu, server: Arc::new(Server) });
    let b = Arc::new(Brains::new(Library::open_in_memory().unwrap(), f));
    b.add_model(ProviderKind::Ollama, "plain:8b").unwrap();
    b.add_model(ProviderKind::Ollama, "rendered:8b").unwrap();
    b.add_model(ProviderKind::Ollama, "moe:30b").unwrap();
    b.add_model(ProviderKind::Ollama, "ngram:4b").unwrap();
    b.install_engine_advisor();
    b
}

/// A model header: `blocks` blocks of attention and FFN (MoE with `experts` experts when not 0),
/// and an n-gram table of `ngram_rows` rows of 256 when not 0. Q8_0 throughout.
fn synthetic(blocks: u32, experts: u32, ngram_rows: u64) -> ModelHeader {
    use serde_json::json;
    let kv = vec![
        ("general.architecture", json!("testarch")),
        ("testarch.block_count", json!(blocks)),
        ("testarch.embedding_length", json!(2048)),
        ("testarch.attention.head_count", json!(16)),
        ("testarch.attention.head_count_kv", json!(4)),
        ("testarch.context_length", json!(32_768)),
        ("testarch.expert_count", json!(experts)),
        ("testarch.expert_used_count", json!(if experts > 0 { 4 } else { 0 })),
    ];
    let mut t: Vec<(String, u32, Vec<u64>)> = vec![
        ("token_embd.weight".into(), 8, vec![2048, 150_000]),
        ("output.weight".into(), 8, vec![2048, 150_000]),
    ];
    if ngram_rows > 0 {
        t.push(("per_layer_token_embd.weight".into(), 8, vec![256, ngram_rows]));
    }
    for b in 0..blocks {
        for (n, shape) in [("attn_q", vec![2048, 2048]), ("attn_k", vec![2048, 512]), ("attn_v", vec![2048, 512]), ("attn_output", vec![2048, 2048])] {
            t.push((format!("blk.{b}.{n}.weight"), 8, shape));
        }
        if experts > 0 {
            for n in ["ffn_gate_exps", "ffn_up_exps"] {
                t.push((format!("blk.{b}.{n}.weight"), 8, vec![2048, 768, u64::from(experts)]));
            }
            t.push((format!("blk.{b}.ffn_down_exps.weight"), 8, vec![768, 2048, u64::from(experts)]));
        } else {
            for n in ["ffn_gate", "ffn_up"] {
                t.push((format!("blk.{b}.{n}.weight"), 8, vec![2048, 8192]));
            }
            t.push((format!("blk.{b}.ffn_down.weight"), 8, vec![8192, 2048]));
        }
    }
    let tensors: Vec<(&str, u32, Vec<u64>)> = t.iter().map(|(n, ty, s)| (n.as_str(), *ty, s.clone())).collect();
    marlowe_hw::gguf::parse_header(&marlowe_hw::gguf::write_header(&kv, &tensors)[..]).unwrap()
}

fn plain() -> ModelRef {
    "ollama/plain:8b".parse().unwrap()
}

fn rendered() -> ModelRef {
    "ollama/rendered:8b".parse().unwrap()
}

fn user(max_ctx: u32, parallel: u16) -> ContextPolicy {
    ContextPolicy { max_ctx, kv_type: KvType::Q8_0, parallel, source: PolicySource::User, updated_ms: 0, placement: Default::default() }
}

#[test]
fn every_local_model_is_on_the_engine_and_one_llama_cpp_cannot_run_says_why() {
    let b = brains(Models::new(), 20_000.0);
    assert!(b.engine_wanted(&plain()));
    assert!(!b.engine_wanted(&rendered()));
    let cant = b.context_plan(&rendered()).unwrap();
    assert_eq!(cant.in_effect.engine, Engine::LlamaCpp, "no model is left to Ollama");
    assert_eq!(cant.in_effect.engine_note.as_deref(), Some("llama.cpp can't run this model: Ollama renders its prompt with its own code (renderer qwen3.5)"));
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
fn the_old_switch_back_to_ollama_is_refused() {
    let b = brains(Models::new(), 20_000.0);
    let e = b.set_model_engine(&plain(), false).unwrap_err();
    assert!(e.to_string().contains("every local model runs on the llama.cpp engine"), "{e}");
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some());
    b.set_model_engine(&plain(), true).unwrap();
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

#[test]
fn a_model_that_does_not_fit_the_free_gpu_runs_split_and_says_why() {
    // A 12b model at q4 needs far more than 5 GiB: `-ngl 99` would take it anyway and the driver
    // would page it into system memory. llama.cpp places what fits instead, as Ollama would.
    let b = brains(Models::new(), 5_000.0);
    let ears = Arc::new(Ears::default());
    b.factory().allocations().set_engine_observer(ears.clone());
    assert!(!b.engine_wanted(&plain()));
    let shape = b.factory().allocations().engine_shape("plain:8b").expect("it still runs, split");
    assert!(shape.split);
    assert_eq!((shape.slots, shape.ctx_per_slot, shape.kv.as_str()), (1, 8_192, "q8_0"), "one slot of its default window");
    assert!(b.factory().allocations().engine_need_mib("plain:8b").is_none());
    let plan = b.context_plan(&plain()).unwrap();
    assert_eq!(plan.in_effect.engine, Engine::LlamaCpp);
    let note = plan.in_effect.engine_note.expect("the reason is on the plan");
    assert!(note.starts_with("llama.cpp runs it split between the GPU and the CPU: needs ") && note.contains(" GiB on the GPU, 4.9 GiB free"), "{note}");
    // The user hears it once, in one line, however many requests ask.
    for _ in 0..3 {
        let _ = b.factory().allocations().engine_shape("plain:8b");
    }
    let heard = ears.0.lock().unwrap().clone();
    assert_eq!(heard.len(), 1, "{heard:?}");
    assert!(heard[0].starts_with("plain:8b runs split between the GPU and the CPU: needs ") && heard[0].ends_with(" GiB on the GPU, 4.9 GiB free"), "{heard:?}");
}

#[test]
fn the_engine_never_runs_a_model_whole_with_a_window_under_its_minimum() {
    // Between "the weights barely fit" and "room to spare" the engine either gives every agent a
    // usable window on the GPU or runs the model split with a real one.
    let mut split = 0;
    for free in (5_000..=20_000).step_by(250) {
        let b = brains(Models::new(), f64::from(free));
        let shape = b.factory().allocations().engine_shape("plain:8b").expect("every local model runs");
        if shape.split {
            split += 1;
        } else {
            assert!(shape.ctx_per_slot >= marlowe_brains::engine::ENGINE_MIN_CTX, "{free} MiB free: {shape:?}");
        }
    }
    assert!(split > 0 && split < 61, "the sweep crosses the fit line: {split} of 61 split");
}

#[test]
fn a_saved_policy_that_does_not_fit_runs_split_with_that_policy() {
    let b = brains(Models::new(), 12_500.0);
    assert!(b.engine_wanted(&plain()));
    // Six slots of 128k are far more cache than the card has left beside the weights.
    b.context().set_policy(&plain(), user(131_072, 6)).unwrap();
    assert!(!b.engine_wanted(&plain()));
    let shape = b.factory().allocations().engine_shape("plain:8b").unwrap();
    assert_eq!((shape.split, shape.slots, shape.ctx_per_slot), (true, 6, 131_072));
}

#[test]
fn no_gpu_reading_is_not_a_fit() {
    let gpu = Arc::new(Dial(Mutex::new(None)));
    let b = brains_on(Models::new(), gpu);
    assert!(!b.engine_wanted(&plain()));
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some_and(|s| s.split));
    let note = b.context_plan(&plain()).unwrap().in_effect.engine_note.expect("a reason");
    assert_eq!(note, "llama.cpp runs it split between the GPU and the CPU: there is no reading of the GPU to fit it against");
}

#[test]
fn what_runs_split_is_looked_at_again_when_the_gpu_frees_up() {
    let gpu = Arc::new(Dial(Mutex::new(Some(5_000.0))));
    let b = brains_on(Models::new(), gpu.clone());
    b.set_engine_recheck(std::time::Duration::ZERO);
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some_and(|s| s.split));
    *gpu.0.lock().unwrap() = Some(20_000.0);
    let shape = b.factory().allocations().engine_shape("plain:8b").expect("room now: the engine takes it whole");
    assert_eq!((shape.slots, shape.split), (6, false));
    assert!(b.factory().allocations().engine_need_mib("plain:8b").is_some_and(|n| n > 4_000.0), "the planner's estimate goes to the engine");
    // Once it fits it keeps its shape: less free memory later does not restart the server with another.
    *gpu.0.lock().unwrap() = Some(14_000.0);
    assert_eq!(b.factory().allocations().engine_shape("plain:8b"), Some(shape));
}

#[test]
fn the_engine_is_told_to_require_the_planners_estimate_and_its_reserve() {
    let b = brains(Models::new(), 20_000.0);
    let shape = b.factory().allocations().engine_shape("plain:8b").expect("fits");
    let plan = b.context_plan(&plain()).unwrap().plan.expect("a plan");
    let kv = KvType::parse(&shape.kv).expect("a cache type");
    let planned = plan.need_mib(shape.ctx_per_slot, kv, shape.slots);
    let told = b.factory().allocations().engine_need_mib("plain:8b").expect("an estimate");
    // The start is held to the room the verdict required, not the bare estimate: a fit kept from a
    // roomier moment must not start in the margin the planner keeps for its own error.
    assert!((told - (planned + marlowe_brains::engine::ENGINE_RESERVE_MIB)).abs() < 1.0, "{told} vs {planned}");
}

#[test]
fn a_load_the_engine_gave_up_is_not_tried_again_until_memory_comes_back() {
    let gpu = Arc::new(Dial(Mutex::new(Some(20_000.0))));
    let b = brains_on(Models::new(), gpu.clone());
    b.set_engine_recheck(std::time::Duration::ZERO);
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some());
    // The server loaded and the driver had moved part of it into system memory.
    b.factory().allocations().engine_declined("plain:8b", "the driver moved 3.0 GiB of it into system memory");
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some_and(|s| s.split));
    let note = b.context_plan(&plain()).unwrap().in_effect.engine_note.unwrap();
    assert_eq!(note, "llama.cpp runs it split between the GPU and the CPU: the driver moved 3.0 GiB of it into system memory");
    // The planner would still say it fits; nothing has been given back, so it stays split.
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some_and(|s| s.split));
    *gpu.0.lock().unwrap() = Some(22_000.0);
    assert!(b.factory().allocations().engine_shape("plain:8b").is_some_and(|s| !s.split), "2 GiB more is free: whole again");
}

#[test]
fn turning_the_engine_on_again_clears_a_gave_up_load() {
    let b = brains(Models::new(), 20_000.0);
    b.factory().allocations().engine_declined("plain:8b", "needs 30.0 GiB on the GPU, 19.5 GiB free");
    assert!(!b.engine_wanted(&plain()));
    b.set_model_engine(&plain(), true).unwrap();
    assert!(b.engine_wanted(&plain()));
}

/// A card with the engine's own server on it: the card is nearly full, and most of it is the
/// server's, which stops before the next model starts.
struct Held;
impl GpuSource for Held {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        let mut s = Gpu(5_000.0).snapshot()?;
        s.gpu.runners_mib = 14_000.0;
        s.gpu.used_mib = 19_576.0;
        s.runners = vec![(4242, 14_000.0)];
        Some(s)
    }
}

#[test]
fn an_engine_model_is_admitted_against_the_memory_its_own_server_gives_back() {
    use marlowe_brains::{Admission, Priority};
    let b = brains_on(Models::new(), Arc::new(Held));
    assert!(b.engine_wanted(&plain()), "free plus the runner's memory fits it");
    assert!(matches!(b.admit(&plain(), Priority::Foreground), Admission::Load(_)), "no question asked: the engine stops what is there first");
}

/// A 16 GiB card with `free` MiB free and `ram` MiB of system RAM free.
struct Card {
    free: f64,
    ram: f64,
}
impl GpuSource for Card {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        let mut s = Gpu(self.free).snapshot()?;
        s.gpu.total_mib = 16_376.0;
        s.gpu.used_mib = 16_376.0 - self.free;
        s.ram_free_mib = Some(self.ram);
        Some(s)
    }
}

fn moe() -> ModelRef {
    "ollama/moe:30b".parse().unwrap()
}

#[test]
fn an_moe_that_does_not_fit_keeps_its_layers_on_the_gpu_and_its_first_experts_on_the_cpu() {
    // 48 blocks of 64 experts: about 17 GiB on the GPU whole, 15 of it experts, on a 14 GiB card.
    let b = brains_on(Models::new(), Arc::new(Card { free: 14_000.0, ram: 40_000.0 }));
    assert!(b.engine_wanted(&moe()), "not a layer split");
    let shape = b.factory().allocations().engine_shape("moe:30b").expect("the engine serves it");
    assert!(!shape.split && shape.cpu_moe > 0 && shape.cpu_moe < 48, "{shape:?}");
    assert!(!shape.host_ram, "the input layer is memory-mapped by default");
    // The engine is held to the GPU part, under the card's free memory.
    let need = b.factory().allocations().engine_need_mib("moe:30b").unwrap();
    assert!(need <= 14_000.0 - 1_024.0, "{need}");
    // Admitted against the VRAM for the GPU part and the RAM for the experts.
    use marlowe_brains::{Admission, Priority};
    match b.admit(&moe(), Priority::Crew) {
        Admission::Load(c) => {
            assert!(c.need_mib.unwrap() <= c.free_mib.unwrap());
            assert!(c.ram_need_mib.unwrap() > 1024.0, "the experts are in RAM: {c:?}");
        }
        other => panic!("{other:?}"),
    }
    // Experts kept on the GPU: the old layer split.
    let keep = ContextPolicy { placement: Placement { experts: Experts::Gpu, ..Default::default() }, ..b.context().policy(&moe()).unwrap() };
    b.context().set_policy(&moe(), keep).unwrap();
    let shape = b.factory().allocations().engine_shape("moe:30b").unwrap();
    assert!(shape.split && shape.cpu_moe == 0, "{shape:?}");
    // All on the CPU, the input layer in RAM.
    let all = ContextPolicy { placement: Placement { experts: Experts::Cpu, host: HostTables::Ram }, ..keep };
    b.context().set_policy(&moe(), all).unwrap();
    let shape = b.factory().allocations().engine_shape("moe:30b").unwrap();
    assert_eq!((shape.split, shape.cpu_moe, shape.host_ram), (false, 48, true));
}

#[test]
fn experts_that_do_not_fit_in_free_ram_are_refused_to_the_crew_and_asked_of_the_user() {
    use marlowe_brains::{Admission, Priority};
    let b = brains_on(Models::new(), Arc::new(Card { free: 14_000.0, ram: 1_000.0 }));
    match b.admit(&moe(), Priority::Crew) {
        Admission::Refused(why) => assert!(why.contains("in RAM") && why.contains("1.0 GiB is free"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(b.admit(&moe(), Priority::Foreground), Admission::Ask(_)));
}

#[test]
fn a_large_ngram_table_is_not_counted_against_the_gpu() {
    use marlowe_brains::{Admission, Priority};
    // 1.3 GiB of weights beside a 4 GiB n-gram table, on a card with 5 GiB free and 2 GiB of RAM:
    // the table is on disk, memory-mapped, and costs neither.
    let b = brains_on(Models::new(), Arc::new(Card { free: 5_000.0, ram: 2_000.0 }));
    let m: ModelRef = "ollama/ngram:4b".parse().unwrap();
    assert!(b.engine_wanted(&m), "{:?}", b.context_plan(&m).unwrap().in_effect.engine_note);
    let plan = b.context_plan(&m).unwrap().plan.unwrap();
    assert!(plan.host_tables_mib > 4_000.0 && plan.weights_main_mib < 2_000.0, "{} {}", plan.host_tables_mib, plan.weights_main_mib);
    assert!(matches!(b.admit(&m, Priority::Crew), Admission::Load(_)));
    // Loaded into RAM instead it no longer fits the 2 GiB free.
    let ram = ContextPolicy { placement: Placement { host: HostTables::Ram, ..Default::default() }, ..b.context().policy(&m).unwrap() };
    b.context().set_policy(&m, ram).unwrap();
    assert!(matches!(b.admit(&m, Priority::Crew), Admission::Refused(_)));
}

