//! The planner's view of the engine: the model files (from Ollama's store, the Hugging Face cache
//! or a model folder, the draft and the projector the engine runs beside them included), what the engine runs now with its slots
//! and its own log, and the server change the planner applies by restarting the engine.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use marlowe_hw::{gguf, log, ModelFiles, ModelSource, RunnerObservation, ServerChange, ServerControl, SlotObservation};
use serde_json::Value;

use super::eligible::resolve;
use super::engine::{same_model, LlamaEngine};
use super::settings::ServerSettings;
use crate::http::{self, LocalEndpoint};
use crate::sources::Sources;

const QUICK: Duration = Duration::from_millis(1500);
const LOG_TAIL_BYTES: u64 = 4 << 20;

/// The engine and the model sources as one [`ModelSource`]. Ollama's server is never asked anything.
pub struct LlamaModels {
    engine: LlamaEngine,
    store: Sources,
}

impl LlamaModels {
    pub fn new(engine: LlamaEngine, store: impl Into<Sources>) -> Self {
        Self { engine, store: store.into() }
    }
}

/// `GET /slots` on a llama-server.
pub fn parse_slots(v: &Value) -> Vec<SlotObservation> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|s| SlotObservation {
            id: s.get("id").and_then(Value::as_u64).unwrap_or(0) as u32,
            n_ctx: s.get("n_ctx").and_then(Value::as_u64).unwrap_or(0) as u32,
            n_prompt_tokens: s.get("n_prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            is_processing: s.get("is_processing").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

impl ModelSource for LlamaModels {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        let m = resolve(&self.store, model_id).map_err(|why| format!("{model_id} can't run: {why}"))?;
        let main = gguf::read_header(&m.blob).map_err(|e| format!("{}: {e}", m.blob.display()))?;
        let draft = m.draft.as_ref().map(|d| gguf::read_header(&d.path).map_err(|e| format!("draft {}: {e}", d.path.display()))).transpose()?;
        let mmproj_bytes = m.mmproj.as_deref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |md| md.len());
        Ok(ModelFiles { main, draft, mmproj_bytes, blob: Some(m.blob.display().to_string()), source: "GGUF header".into(), quant: None })
    }

    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        let mut rt = RunnerObservation::default();
        let Some(st) = self.engine.status().filter(|s| same_model(&s.model, model_id)) else { return rt };
        rt.loaded = true;
        rt.ps_context = Some(st.shape.ctx_per_slot);
        let ep = LocalEndpoint::new("127.0.0.1", st.port);
        rt.slots = ep
            .and_then(|ep| http::get_json(&ep, "/slots", QUICK).ok())
            .map(|v| parse_slots(&v))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                (0..u32::from(st.shape.slots))
                    .map(|id| SlotObservation { id, n_ctx: st.shape.ctx_per_slot, n_prompt_tokens: 0, is_processing: false })
                    .collect()
            });
        // This server's own load: the buffers it printed, for the planner to calibrate against.
        let blocks = log::parse(&self.engine.log_tail(LOG_TAIL_BYTES));
        if let Some(b) = blob.and_then(|b| log::last_for_blob(&blocks, b)) {
            rt.load = Some(b.clone());
        }
        rt
    }

    fn loaded_models(&self) -> Vec<String> {
        self.engine.status().map(|s| vec![s.model]).unwrap_or_default()
    }

    /// The engine answers for itself: what it runs is always known.
    fn resident(&self) -> Option<Vec<String>> {
        Some(self.loaded_models())
    }

    fn unload(&self, model_id: &str) -> Result<(), String> {
        self.engine.unload(Some(model_id));
        Ok(())
    }

    fn runs_on_own_server(&self, model_id: &str) -> bool {
        self.engine.serves(model_id)
    }

    fn model_folders(&self) -> Vec<PathBuf> {
        self.store.dirs()
    }

    fn restart_engine(&self) {
        self.engine.restart_when_idle();
    }

    fn ms_models(&self) -> Vec<(String, u8)> {
        self.store
            .scan()
            .into_iter()
            .filter_map(|f| {
                let m = super::eligible::resolve_found(&self.store, &f).ok()?;
                Some((f.display, m.ms?.generation?.number()))
            })
            .collect()
    }

    fn add_model_dir(&self, path: &std::path::Path) -> Result<bool, String> {
        Ok(self.store.add_dir(path.to_path_buf()))
    }

    fn engine_eligibility(&self, model_id: &str) -> Result<(), String> {
        resolve(&self.store, model_id).map(|_| ())
    }
}

/// Applies the planner's server change to the engine: flash attention is the engine's own setting
/// (saved through `save`); the KV type and the slots are the model's policy, already saved. The
/// server restarts with them: it saves its slot caches and stops, and the next request starts it.
pub struct EngineControl {
    engine: LlamaEngine,
    save: Arc<dyn Fn(&ServerSettings) -> Result<(), String> + Send + Sync>,
    log: Option<PathBuf>,
}

impl EngineControl {
    pub fn new(engine: LlamaEngine, save: Arc<dyn Fn(&ServerSettings) -> Result<(), String> + Send + Sync>) -> Self {
        Self { engine, save, log: None }
    }

    /// The engine's log, for the live meter.
    pub fn with_log(mut self, path: PathBuf) -> Self {
        self.log = Some(path);
        self
    }
}

impl ServerControl for EngineControl {
    fn apply(&self, change: &ServerChange) -> Result<String, String> {
        if change.is_empty() {
            return Ok("nothing to change".into());
        }
        let mut what: Vec<String> = Vec::new();
        if let Some(on) = change.flash_attn {
            let s = ServerSettings { flash_attn: on, ..self.engine.settings() };
            (self.save)(&s)?;
            self.engine.set_settings(s);
            what.push(format!("flash attention {}", if on { "on" } else { "off" }));
        }
        if let Some(k) = change.kv_type {
            what.push(format!("KV cache {}", k.ollama_name()));
        }
        if let Some(n) = change.parallel {
            what.push(format!("{n} slot{}", if n == 1 { "" } else { "s" }));
        }
        let stopped = self.engine.unload(None);
        Ok(format!(
            "llama.cpp {} with {}",
            if stopped { "restarts on the next request" } else { "starts on the next request" },
            what.join(", ")
        ))
    }

    fn log_feed(&self) -> Option<Box<dyn marlowe_hw::LogFeed>> {
        self.log.as_deref().map(|p| Box::new(crate::server_log::EngineLogFeed::new(p)) as Box<dyn marlowe_hw::LogFeed>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::engine::Want;
    use crate::llama::testkit::Fixture;
    use marlowe_model::{CancelToken, EngineShape, Lane};

    const MODEL: &str = "dusk:22b";

    fn source(fx: &Fixture, engine: &LlamaEngine) -> LlamaModels {
        LlamaModels::new(engine.clone(), fx.dir.path().join("store"))
    }

    fn want(slots: u16) -> Want {
        Want { model: MODEL.into(), shape: EngineShape { slots, ctx_per_slot: 8192, kv: "q8_0".into(), split: false, cpu_moe: 0, host_ram: false }, lane: Lane::Conductor, key: 1, need_mib: None }
    }

    #[test]
    fn a_model_the_engine_runs_is_loaded_with_its_slots_and_its_own_buffers() {
        let fx = Fixture::new(MODEL);
        let engine = fx.engine();
        let hw = source(&fx, &engine);
        let before = hw.runtime(MODEL, None);
        assert!(!before.loaded && before.slots.is_empty());
        assert_eq!(hw.resident(), Some(vec![]), "nothing loaded is known, not unknown");
        let _lease = engine.acquire(&want(4), &CancelToken::new()).unwrap();
        let rt = hw.runtime(MODEL, None);
        assert!(rt.loaded);
        assert_eq!((rt.slots.len(), rt.ps_context), (4, Some(8192)));
        assert!(rt.slots.iter().all(|s| s.n_ctx == 8192), "{:?}", rt.slots);
        assert_eq!(hw.resident(), Some(vec![MODEL.to_string()]));
        assert_eq!(hw.loaded_models(), vec![MODEL.to_string()]);
    }

    #[test]
    fn unloading_a_model_stops_the_server() {
        let fx = Fixture::new(MODEL);
        let engine = fx.engine();
        let hw = source(&fx, &engine);
        drop(engine.acquire(&want(2), &CancelToken::new()).unwrap());
        hw.unload(MODEL).unwrap();
        assert!(engine.status().is_none());
    }

    #[test]
    fn the_files_are_read_from_the_store_with_the_draft_and_projector_the_engine_runs() {
        let fx = Fixture::with_layers("dawn:9b", true, true);
        let engine = fx.engine();
        let hw = source(&fx, &engine);
        let f = hw.files("dawn:9b").unwrap();
        assert_eq!(f.main.arch(), Some("qwen35"));
        assert!(f.draft.is_some() && f.mmproj_bytes > 0 && f.source == "GGUF header");
        assert_eq!(hw.engine_eligibility("dawn:9b"), Ok(()));
        assert!(hw.engine_eligibility("missing:1").is_err());
    }

    #[test]
    fn model_files_from_a_folder_are_planned_by_their_real_sizes_and_a_folder_added_later_is_searched() {
        use crate::sources::Sources;
        let fx = Fixture::new(MODEL);
        let folder = fx.dir.path().join("models");
        let ms = marlowe_hw::gguf::write_gguf(
            &[
                ("general.architecture", serde_json::json!("qwen35")),
                ("tokenizer.chat_template", serde_json::json!(crate::llama::eligible::tests::TEMPLATE)),
                ("msffn.quant_name", serde_json::json!("MS2.7")),
            ],
            &[("blk.0.ffn_up.weight", 46, vec![256], 4096), ("blk.0.attn_norm.weight", 0, vec![8], 32)],
        );
        crate::sources::tests::put(&folder.join("Qwen-MS.gguf"), &ms);
        let sources = Sources::new(fx.dir.path().join("store"), Vec::new(), Vec::new());
        let hw = LlamaModels::new(fx.engine(), sources.clone());
        assert!(hw.ms_models().is_empty() && hw.files("gguf/Qwen-MS").is_err());
        assert_eq!(hw.add_model_dir(&folder), Ok(true));
        assert_eq!(hw.add_model_dir(&folder), Ok(false));
        assert_eq!(hw.ms_models(), vec![("Qwen-MS".to_string(), 1)]);
        // The MS tensor is sized by the file, not counted as nothing.
        let f = hw.files("gguf/Qwen-MS").unwrap();
        assert_eq!(f.main.tensors[0].bytes(), 4096);
        assert_eq!(f.main.tensors.iter().map(|t| t.bytes()).sum::<u64>() > 4096, true);
    }

    #[test]
    fn the_planners_apply_saves_the_setting_and_restarts_the_engine() {
        let fx = Fixture::new(MODEL);
        let engine = fx.engine();
        drop(engine.acquire(&want(2), &CancelToken::new()).unwrap());
        let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = saved.clone();
        let control = EngineControl::new(engine.clone(), Arc::new(move |s: &ServerSettings| {
            s2.lock().unwrap().push(s.flash_attn);
            Ok(())
        }));
        let change = ServerChange { kv_type: Some(marlowe_hw::KvType::Q4_0), parallel: Some(4), flash_attn: Some(false) };
        let msg = control.apply(&change).unwrap();
        assert_eq!(msg, "llama.cpp restarts on the next request with flash attention off, KV cache q4_0, 4 slots");
        assert!(engine.status().is_none(), "the server stopped");
        assert_eq!(fx.spawner.killed(), 1);
        assert_eq!((saved.lock().unwrap().clone(), engine.settings().flash_attn), (vec![false], false));
        // The next request starts it again with the new settings.
        drop(engine.acquire(&want(4), &CancelToken::new()).unwrap());
        let plan = fx.spawner.plans.lock().unwrap()[1].clone();
        assert!(plan.args.windows(2).any(|p| p == ["--flash-attn", "off"]) && plan.args.windows(2).any(|p| p == ["-np", "4"]));
        assert_eq!(control.ensure_running(&mut |_| {}), marlowe_hw::EngineStart::NotApplicable, "nothing to start ahead of a request");
    }
}
