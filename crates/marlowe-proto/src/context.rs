//! The context and hardware planner on the wire: per-model context policy, the plan the daemon
//! computes from the model file and the GPU, and the changes a saved policy makes.
//!
//! The arithmetic lives in `marlowe-hw` ([`PlanModel`]); the client moves the sliders against
//! the same model the daemon used, so what the popup predicts and what the daemon saves agree.

use marlowe_model::ModelRef;
use serde::{Deserialize, Serialize};

pub use marlowe_hw::plan::{ctx_label, Fit, GpuBudget, PlanModel, MAX_AGENTS};
pub use marlowe_hw::{KvType, LayerRole};

/// The approval tool name for restarting the Ollama server with new settings. The client offers
/// Allow and Deny only for it (never "for this session").
pub const SERVER_RESTART_TOOL: &str = "ollama_server_restart";

/// Who chose a policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    /// The planner's automatic choice.
    #[default]
    Auto,
    User,
}

/// The context policy of one model: the window every request allocates (`num_ctx`), the KV
/// type, and how many agents may run on it at once. Agents get `max_ctx` or less, never more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPolicy {
    pub max_ctx: u32,
    pub kv_type: KvType,
    /// Concurrent agents on this model's runner, the conductor included when it shares it.
    pub parallel: u16,
    #[serde(default)]
    pub source: PolicySource,
    #[serde(default)]
    pub updated_ms: i64,
}

/// The smallest window a policy may set.
pub const MIN_CTX: u32 = 2048;

/// Which engine serves the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// Ollama's bundled llama-server: KV type and parallel are server-wide settings.
    #[default]
    Ollama,
    /// A hosted model: the planner offers the context choice only.
    Cloud,
}

/// One runner slot as `/slots` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerSlot {
    pub id: u32,
    pub n_ctx: u32,
    #[serde(default)]
    pub n_prompt_tokens: u32,
    #[serde(default)]
    pub is_processing: bool,
}

/// What the engine runs now (the runner's argv) and what is saved for its next start.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineSettings {
    pub engine: Engine,
    /// The model is loaded right now.
    #[serde(default)]
    pub loaded: bool,
    /// The runner's per-slot context (`-c` / `-np`), when loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// The runner's `-c` as launched (every slot together), from its log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_type: Option<KvType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash_attn: Option<bool>,
    /// Draft positions when speculative decoding runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_n: Option<u32>,
    /// `offloaded N/M layers` of the last load: fewer than all means a partial offload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_layers: Option<(u32, u32)>,
    /// The server settings saved for Ollama's next start (the user environment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_kv_type: Option<KvType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_parallel: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_flash_attn: Option<bool>,
    #[serde(default)]
    pub slots: Vec<RunnerSlot>,
    /// Last measured decode speed, tokens/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_tok_s: Option<f64>,
    /// Seconds the last load took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_secs: Option<f64>,
}

impl EngineSettings {
    /// The KV type the next load will use: what is saved, else what runs.
    pub fn next_kv_type(&self) -> Option<KvType> {
        self.saved_kv_type.or(self.kv_type)
    }

    pub fn next_parallel(&self) -> Option<u16> {
        self.saved_parallel.or(self.parallel)
    }

    pub fn partial(&self) -> bool {
        self.gpu_layers.is_some_and(|(n, m)| n < m)
    }

    /// The tokens one runner slot really holds, when the loaded runner was launched for an
    /// allocation of `want` but gives each slot less: Ollama passing `-c num_ctx -np N` splits
    /// the window N ways. `None` when the slots hold `want` or more, or the runner predates
    /// `want` (it reloads on the next request).
    pub fn split_slot_ctx(&self, want: u32) -> Option<u32> {
        if !self.loaded || self.launch_ctx.is_none_or(|c| c < want) {
            return None;
        }
        self.slots.iter().map(|s| s.n_ctx).filter(|n| *n > 0).min().filter(|n| *n < want)
    }
}

/// The architecture as the popup describes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchSummary {
    pub arch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    /// One role per block, for the layer strip.
    #[serde(default)]
    pub layers: Vec<LayerRole>,
    /// `64 layers, 16 keep a KV cache, 48 Gated DeltaNet, 1 MTP`.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sliding_window: Option<u32>,
    #[serde(default)]
    pub mla: bool,
    /// The draft model, when speculative decoding is set up (`MTP 1 layer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    #[serde(default)]
    pub vision: bool,
    /// Where the header came from: `GGUF header` or `/api/show`.
    #[serde(default)]
    pub source: String,
}

/// One row of the table: a context, per KV type its cost per agent, how many agents fit and how
/// one agent lands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRow {
    pub ctx: u32,
    pub per_agent_mib: [f64; 3],
    pub agents: [Option<u16>; 3],
    pub fit: [Fit; 3],
}

/// The planner's whole answer for one model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPlan {
    pub model: ModelRef,
    /// `None` for a hosted model or when the file could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanModel>,
    #[serde(default)]
    pub arch: ArchSummary,
    #[serde(default)]
    pub rows: Vec<PlanRow>,
    #[serde(default)]
    pub in_effect: EngineSettings,
    /// The saved policy, or the automatic one when nothing is saved.
    pub policy: ContextPolicy,
    /// What `a` fills in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<ContextPolicy>,
    /// The model's own maximum context.
    pub ctx_limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_total_mib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_free_mib: Option<f64>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// When a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeWhen {
    /// On the next request, by Marlowe alone.
    Now,
    /// Needs the Ollama server restarted with new settings (affects every Ollama model).
    Restart,
}

/// One line of the review screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedChange {
    pub when: ChangeWhen,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A model's policy as the meter and the picker need it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelContext {
    pub model: ModelRef,
    /// The window requests allocate (`num_ctx`): the saved policy's, else the provider default.
    pub max_ctx: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ContextPolicy>,
    #[serde(default)]
    pub in_effect: EngineSettings,
}

impl ModelContext {
    /// A saved KV type the running server does not use yet. Compared with what runs, not with
    /// what is saved for the next start: until Ollama restarts, the old type is what the cache
    /// costs. Falls back to the saved setting only when nothing has run since the server started.
    pub fn pending(&self) -> Option<KvType> {
        let p = self.policy?;
        let running = self.in_effect.kv_type.or(self.in_effect.saved_kv_type)?;
        (self.in_effect.engine == Engine::Ollama && p.kv_type != running).then_some(p.kv_type)
    }
}

/// The server-wide settings saving `new` needs that the next Ollama start would not use:
/// (KV type, parallel, flash attention), each `None` when already right. Hosted models need none.
pub fn server_change(in_effect: &EngineSettings, new: &ContextPolicy) -> (Option<KvType>, Option<u16>, Option<bool>) {
    if in_effect.engine != Engine::Ollama {
        return (None, None, None);
    }
    let kv = (in_effect.next_kv_type() != Some(new.kv_type)).then_some(new.kv_type);
    let parallel = (in_effect.next_parallel().unwrap_or(1) != new.parallel).then_some(new.parallel);
    let flash_on = in_effect.saved_flash_attn.or(in_effect.flash_attn).unwrap_or(false);
    let flash = (new.kv_type != KvType::F16 && !flash_on).then_some(true);
    (kv, parallel, flash)
}

/// What saving `new` changes, given the window requests use now (`old_max_ctx`) and what the
/// engine runs. The review screen shows exactly these lines, and the daemon answers with them.
pub fn planned_changes(old_max_ctx: u32, in_effect: &EngineSettings, new: &ContextPolicy) -> Vec<PlannedChange> {
    let now = |text: String| PlannedChange { when: ChangeWhen::Now, text, detail: None };
    let mut v = Vec::new();
    if new.max_ctx != old_max_ctx {
        let reload = match (in_effect.engine, in_effect.load_secs) {
            (Engine::Cloud, _) => String::new(),
            (_, Some(s)) => format!("; Ollama reloads it on the next request (~{s:.0} s)"),
            (_, None) => "; Ollama reloads it on the next request (~6–14 s)".into(),
        };
        v.push(now(format!("Requests to this model use num_ctx {} (was {}){reload}", thousands(new.max_ctx), thousands(old_max_ctx))));
    }
    v.push(now(format!(
        "Agents on this model: at most {} each, never more; at most {} at once",
        ctx_label(new.max_ctx),
        new.parallel
    )));
    let (kv, parallel, flash) = server_change(in_effect, new);
    let running = in_effect.slots.iter().filter(|s| s.is_processing).count();
    let detail = format!(
        "sets your user environment variable, then quits and relaunches Ollama (unloads every loaded Ollama model{}; {running} request{} running on this one)",
        if in_effect.loaded { ", this one included" } else { "" },
        if running == 1 { "" } else { "s" }
    );
    let restart = |text: String| PlannedChange { when: ChangeWhen::Restart, text, detail: Some(detail.clone()) };
    if let Some(k) = kv {
        let was = in_effect.next_kv_type().map_or("f16 (unset)".to_string(), |t| t.ollama_name().to_string());
        v.push(restart(format!("Ollama server, affects every Ollama model: OLLAMA_KV_CACHE_TYPE {was} → {}", k.ollama_name())));
    }
    if let Some(n) = parallel {
        v.push(restart(format!(
            "Ollama server, affects every Ollama model: OLLAMA_NUM_PARALLEL {} → {n}",
            in_effect.next_parallel().unwrap_or(1)
        )));
    }
    if flash.is_some() {
        v.push(restart("Ollama server: OLLAMA_FLASH_ATTENTION → 1 (a quantized KV cache needs flash attention)".into()));
    }
    v
}

fn thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `46k · q8 · 1×`.
pub fn policy_label(p: &ContextPolicy) -> String {
    format!("{} · {} · {}×", ctx_label(p.max_ctx), p.kv_type.short(), p.parallel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_round_trip_and_pending_means_not_running_yet() {
        let p = ContextPolicy { max_ctx: 47_104, kv_type: KvType::Q4_0, parallel: 1, source: PolicySource::User, updated_ms: 1 };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"kv_type\":\"q4_0\""), "{s}");
        assert_eq!(serde_json::from_str::<ContextPolicy>(&s).unwrap(), p);
        let mut mc = ModelContext {
            model: "ollama/m".parse().unwrap(),
            max_ctx: 47_104,
            policy: Some(p),
            in_effect: EngineSettings { kv_type: Some(KvType::Q8_0), ..Default::default() },
        };
        assert_eq!(mc.pending(), Some(KvType::Q4_0));
        // Saved for the next start but the server still runs q8_0: still pending.
        mc.in_effect.saved_kv_type = Some(KvType::Q4_0);
        assert_eq!(mc.pending(), Some(KvType::Q4_0));
        // The server restarted and runs it.
        mc.in_effect.kv_type = Some(KvType::Q4_0);
        assert_eq!(mc.pending(), None);
        assert_eq!(policy_label(&p), "46k · q4 · 1×");
    }

    #[test]
    fn review_lists_now_and_restart_changes() {
        let running = EngineSettings {
            loaded: true,
            kv_type: Some(KvType::Q8_0),
            parallel: Some(1),
            flash_attn: Some(true),
            load_secs: Some(6.8),
            ..Default::default()
        };
        let p = ContextPolicy { max_ctx: 47_104, kv_type: KvType::Q4_0, parallel: 1, source: PolicySource::User, updated_ms: 0 };
        let c = planned_changes(32_768, &running, &p);
        assert_eq!(c.len(), 3, "{c:?}");
        assert_eq!(c[0].text, "Requests to this model use num_ctx 47,104 (was 32,768); Ollama reloads it on the next request (~7 s)");
        assert_eq!(c[1].text, "Agents on this model: at most 46k each, never more; at most 1 at once");
        assert_eq!(c[2].when, ChangeWhen::Restart);
        assert!(c[2].text.ends_with("OLLAMA_KV_CACHE_TYPE q8_0 → q4_0"), "{}", c[2].text);
        assert!(c[2].detail.as_deref().unwrap().contains("unloads every loaded Ollama model, this one included; 0 requests running on this one"));
        assert_eq!(server_change(&running, &p), (Some(KvType::Q4_0), None, None));
        // Already saved for the next start: nothing to restart for.
        let saved = EngineSettings { saved_kv_type: Some(KvType::Q4_0), ..running.clone() };
        assert!(planned_changes(47_104, &saved, &p).iter().all(|c| c.when == ChangeWhen::Now));
        let cloud = EngineSettings { engine: Engine::Cloud, ..Default::default() };
        assert_eq!(planned_changes(200_000, &cloud, &ContextPolicy { max_ctx: 131_072, ..p }).len(), 2);
    }
}
