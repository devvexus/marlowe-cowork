//! The planner's view of the machine with Marlowe's llama-server in it: Ollama's store and
//! runners as before, plus the engine's own model, slots and log.

use std::path::PathBuf;
use std::time::Duration;

use marlowe_hw::log;
use marlowe_hw::{ModelFiles, ModelSource, RunnerObservation, SlotObservation};

use super::eligible::facts_in;
use super::engine::{same_model, LlamaEngine};
use crate::http::{self, LocalEndpoint};
use crate::ollama_runtime::{parse_slots, OllamaHw};

const QUICK: Duration = Duration::from_millis(1500);
const LOG_TAIL_BYTES: u64 = 4 << 20;

/// [`OllamaHw`] and the engine as one [`ModelSource`].
pub struct LlamaModels {
    ollama: OllamaHw,
    engine: LlamaEngine,
    store: PathBuf,
}

impl LlamaModels {
    pub fn new(ollama: OllamaHw, engine: LlamaEngine, store: PathBuf) -> Self {
        Self { ollama, engine, store }
    }
}

impl ModelSource for LlamaModels {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        self.ollama.files(model_id)
    }

    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        let mut rt = self.ollama.runtime(model_id, blob);
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
        self.resident().unwrap_or_default()
    }

    fn resident(&self) -> Option<Vec<String>> {
        let mut v = self.ollama.resident()?;
        if let Some(s) = self.engine.status() {
            if !v.iter().any(|m| same_model(m, &s.model)) {
                v.push(s.model);
            }
        }
        Some(v)
    }

    fn unload(&self, model_id: &str) -> Result<(), String> {
        if self.engine.unload(Some(model_id)) {
            return Ok(());
        }
        self.ollama.unload(model_id)
    }

    fn runs_on_own_server(&self, model_id: &str) -> bool {
        self.engine.serves(model_id)
    }

    fn engine_eligibility(&self, model_id: &str) -> Result<(), String> {
        match facts_in(&self.store, model_id)?.why_not() {
            None => Ok(()),
            Some(why) => Err(why),
        }
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
        let store = fx.dir.path().join("store");
        let ollama = OllamaHw::with_paths(fx.ollama.endpoint(), store.clone(), None);
        LlamaModels::new(ollama, engine.clone(), store)
    }

    #[test]
    fn a_model_the_engine_runs_is_loaded_with_its_slots_and_its_own_buffers() {
        let fx = Fixture::new(MODEL);
        let engine = fx.engine();
        let hw = source(&fx, &engine);
        let before = hw.runtime(MODEL, None);
        assert!(!before.loaded && before.slots.is_empty());
        assert_eq!(hw.resident(), Some(vec![]));
        let want = Want { model: MODEL.into(), shape: EngineShape { slots: 4, ctx_per_slot: 8192, kv: "q8_0".into() }, lane: Lane::Conductor, key: 1 };
        let _lease = engine.acquire(&want, &CancelToken::new()).unwrap();
        let rt = hw.runtime(MODEL, None);
        assert!(rt.loaded);
        assert_eq!((rt.slots.len(), rt.ps_context), (4, Some(8192)));
        assert!(rt.slots.iter().all(|s| s.n_ctx == 8192), "{:?}", rt.slots);
        assert_eq!(hw.resident(), Some(vec![MODEL.to_string()]));
        assert_eq!(hw.loaded_models(), vec![MODEL.to_string()]);
    }

    #[test]
    fn unloading_a_model_the_engine_runs_stops_the_server_not_ollama() {
        let fx = Fixture::new(MODEL);
        let engine = fx.engine();
        let hw = source(&fx, &engine);
        let want = Want { model: MODEL.into(), shape: EngineShape { slots: 2, ctx_per_slot: 8192, kv: "q8_0".into() }, lane: Lane::Conductor, key: 1 };
        drop(engine.acquire(&want, &CancelToken::new()).unwrap());
        hw.unload(MODEL).unwrap();
        assert!(engine.status().is_none());
        assert!(fx.ollama.unloaded().is_empty() || fx.ollama.unloaded() == vec![MODEL.to_string()], "only the pre-load unload");
    }

    #[test]
    fn eligibility_is_the_manifests() {
        let fx = Fixture::new(MODEL);
        let engine = fx.engine();
        let hw = source(&fx, &engine);
        assert_eq!(hw.engine_eligibility(MODEL), Ok(()));
        assert!(hw.engine_eligibility("missing:1").is_err());
    }
}
