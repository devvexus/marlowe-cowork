//! The context planner against this machine: the real model store, the engine's own log, NVML
//! and PDH. Ignored by default (machine-specific). Loads nothing, restarts nothing, changes no
//! environment: it reads the load of `MARLOWE_LIVE_MODEL` that Marlowe's engine (running under
//! `MARLOWE_HOME`) logged last, if any.
//!
//! ```text
//! powershell -NoProfile -File scripts/cargo-idle.ps1 test -p marlowe --test context_live -j 2 -- --ignored --nocapture
//! ```

use std::sync::Arc;

use marlowe_brains::{Brains, Hardware, Library, MemoryStore, ProviderFactory};
use marlowe_hw::arch::{ArchProfile, Role};
use marlowe_hw::{build, log, predict, CacheType, Calibration, GpuSource, KvType, ModelSource, PlanInputs, ServerChange, ServerControl};
use marlowe_model::{ModelInfo, ModelRef, ProviderKind};
use marlowe_provider::llama::{EngineConfig, LlamaEngine, LlamaModels, SystemSpawner};
use marlowe_provider::ollama_store::store_root;
use marlowe_sentinel::vram::SentinelGpu;

struct Refuse;
impl ServerControl for Refuse {
    fn apply(&self, _: &ServerChange) -> Result<String, String> {
        Err("this test never restarts the engine".into())
    }
}

#[test]
#[ignore]
fn prediction_against_pdh_and_the_engines_log() {
    let (Ok(name), Some(home)) = (std::env::var("MARLOWE_LIVE_MODEL"), std::env::var_os("MARLOWE_HOME")) else {
        eprintln!("set MARLOWE_LIVE_MODEL and MARLOWE_HOME to a model Marlowe's engine has loaded (this test never loads one)");
        return;
    };
    let engine = LlamaEngine::new(EngineConfig::new(store_root()), Arc::new(SystemSpawner::new(Vec::new())));
    let hw = LlamaModels::new(engine, store_root());
    let gpu = SentinelGpu::open();
    let files = hw.files(&name).expect("the store resolves the model");
    let text = marlowe_provider::server_log::read_tail(&std::path::Path::new(&home).join("logs").join("llama-server.log"), 16 << 20).expect("the engine's log");
    let blocks = log::parse(&text);
    let snap = gpu.snapshot().expect("an NVIDIA GPU");
    let main = ArchProfile::analyze(&files.main, Role::Main).unwrap();
    let draft = files.draft.as_ref().map(|d| ArchProfile::analyze(d, Role::Draft).unwrap());
    let load = files.blob.as_deref().and_then(|b| log::last_for_blob(&blocks, b)).cloned().filter(|l| !l.contexts.is_empty()).expect("the engine's log has this load");
    let m = ModelInfo::bare(ProviderKind::Ollama, &name);
    let l = &load.launch;
    let kv = l.cache_k.as_deref().and_then(CacheType::parse).unwrap_or(CacheType::F16);
    let ctx = l.ctx.unwrap_or(4096);
    let np = l.parallel.unwrap_or(1);
    let spec = l.spec_n.unwrap_or(0);
    let ub = l.ubatch.unwrap_or(512);

    eprintln!("model {} · ctx {ctx} · np {np} · kv {} · draft {} (n={spec}) · ub {ub}", m.id, kv.name(), draft.is_some());
    eprintln!("arch  {}", main.summary());

    // Buffer by buffer against the runner's own log.
    let p = predict(&main, draft.as_ref(), ctx / np, np, kv, CacheType::F16, spec, ub);
    let (lm, ld) = (load.main().unwrap(), load.draft());
    let row = |name: &str, want: f64, got: f64| eprintln!("  {name:<26} predicted {want:>10.2}  log {got:>10.2}  error {:+.2}%", (want - got) / got.max(1e-9) * 100.0);
    eprintln!("buffers (MiB):");
    row("main weights, GPU", p.main_gpu_weights, lm.model.iter().filter(|(d, _)| log::is_gpu_device(d)).map(|(_, v)| v).sum());
    row("main weights, host", p.main_host_weights, lm.model.iter().filter(|(d, _)| !log::is_gpu_device(d)).map(|(_, v)| v).sum());
    row("main KV", p.main_kv, lm.kv_total());
    if let Some(rs) = &lm.rs_line {
        row("recurrent state", p.main_state, rs.mib);
    }
    if let Some(d) = ld {
        row("draft weights, GPU", p.draft_gpu_weights, d.model.iter().filter(|(k, _)| log::is_gpu_device(k)).map(|(_, v)| v).sum());
        row("draft KV", p.draft_kv, d.kv_total());
    }

    // The whole server: uncalibrated prediction vs PDH (dedicated).
    let plan = build(&PlanInputs {
        main: &main,
        draft: draft.as_ref(),
        spec_n: spec,
        draft_kv: CacheType::F16,
        ubatch: ub,
        mmproj_b: files.mmproj_bytes,
        gpu: Some(snap.gpu.clone()),
        calib: Calibration::default(),
        placement: Default::default(),
    });
    let kvt = KvType::from_cache(kv).unwrap_or(KvType::F16);
    let need = plan.need_mib(ctx / np, kvt, np as u16);
    let dedicated: f64 = snap.runners.iter().map(|r| r.1).sum();
    eprintln!("whole runner (MiB):");
    eprintln!("  log, every GPU buffer      {:>10.1}", load.gpu_mib());
    eprintln!("  predicted (uncalibrated)   {need:>10.1}   vs PDH {:+.2}%", (need - dedicated) / dedicated * 100.0);
    eprintln!("  PDH dedicated              {dedicated:>10.1}   (runners {:?})", snap.runners);
    eprintln!("  driver overhead measured   {:>10.1}", dedicated - load.gpu_mib());
    eprintln!("  NVML: used {:.0}, free {:.0}, others {:.0}, budget {:.0}, bandwidth {:?} GB/s", snap.gpu.used_mib, snap.gpu.free_mib, snap.gpu.others_mib(), snap.gpu.budget_mib(), snap.gpu.bandwidth_gbs);

    // And through the library, as the client sees it.
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.set_hardware(Hardware { models: Arc::new(hw), gpu: Arc::new(gpu), server: Arc::new(Refuse) });
    let lib = Library::open_in_memory().unwrap();
    lib.add(&m).unwrap();
    let b = Brains::new(lib, f);
    let r: ModelRef = format!("ollama/{}", m.id).parse().unwrap();
    let cp = b.context_plan(&r).unwrap();
    let pm = cp.plan.as_ref().unwrap();
    eprintln!("plan: calibrated {} · fixed at {ctx}: {:.0} MiB · auto {:?} · in effect {:?}", pm.calibrated, pm.fixed_mib(ctx), cp.auto, cp.in_effect.kv_type);
    for row in &cp.rows {
        eprintln!("  {:>7}  per agent {:>7.0} MiB  agents f16/q8/q4 {:?}  fit {:?}", row.ctx, row.per_agent_mib[kvt.index()], row.agents, row.fit[kvt.index()]);
    }
    for w in &cp.warnings {
        eprintln!("  ! {w}");
    }
    assert!(((need - dedicated) / dedicated).abs() < 0.05, "the uncalibrated prediction is within 5% of PDH");
}
