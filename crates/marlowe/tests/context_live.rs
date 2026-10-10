//! The context planner against this machine: the real Ollama store, runner log, `/api/ps`, NVML
//! and PDH. Ignored by default (machine-specific). Loads nothing, restarts nothing, changes no
//! environment: it reads the model that is already loaded, if any.
//!
//! ```text
//! powershell -NoProfile -File scripts/cargo-idle.ps1 test -p marlowe --test context_live -j 2 -- --ignored --nocapture
//! ```

use std::sync::Arc;

use marlowe_brains::{Brains, Hardware, Library, MemoryStore, ProviderFactory};
use marlowe_hw::arch::{ArchProfile, Role};
use marlowe_hw::{build, log, predict, CacheType, Calibration, GpuSource, KvType, ModelSource, PlanInputs, ServerChange, ServerControl};
use marlowe_model::{ModelInfo, ModelRef, ProviderKind};
use marlowe_provider::{ollama_runtime, LocalEndpoint, OllamaHw};
use marlowe_sentinel::vram::SentinelGpu;

struct Refuse;
impl ServerControl for Refuse {
    fn apply(&self, _: &ServerChange) -> Result<String, String> {
        Err("this test never restarts Ollama".into())
    }
}

#[test]
#[ignore]
fn prediction_against_pdh_and_api_ps() {
    let ep = LocalEndpoint::default_ollama();
    let ps = marlowe_provider::http::get_json(&ep, "/api/ps", std::time::Duration::from_secs(3)).expect("Ollama answers /api/ps");
    let loaded = ollama_runtime::parse_ps(&ps);
    let Some(m) = loaded.first() else {
        eprintln!("no model is loaded; nothing to measure (this test never loads one)");
        return;
    };
    let hw = OllamaHw::new(ep.clone());
    let gpu = SentinelGpu::open();
    let files = hw.files(&m.name).expect("the store resolves the loaded model");
    let rt = hw.runtime(&m.name, files.blob.as_deref());
    let snap = gpu.snapshot().expect("an NVIDIA GPU");
    let main = ArchProfile::analyze(&files.main, Role::Main).unwrap();
    let draft = files.draft.as_ref().map(|d| ArchProfile::analyze(d, Role::Draft).unwrap());
    let load = rt.load.clone().filter(|l| !l.contexts.is_empty()).expect("the runner log has this load");
    let l = &load.launch;
    let kv = l.cache_k.as_deref().and_then(CacheType::parse).unwrap_or(CacheType::F16);
    let ctx = l.ctx.unwrap_or(4096);
    let np = l.parallel.unwrap_or(1);
    let spec = l.spec_n.unwrap_or(0);
    let ub = l.ubatch.unwrap_or(512);

    eprintln!("model {} · ctx {ctx} · np {np} · kv {} · draft {} (n={spec}) · ub {ub}", m.name, kv.name(), draft.is_some());
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

    // The whole runner: uncalibrated prediction vs PDH (dedicated) vs /api/ps.
    let plan = build(&PlanInputs {
        main: &main,
        draft: draft.as_ref(),
        spec_n: spec,
        draft_kv: CacheType::F16,
        ubatch: ub,
        mmproj_b: files.mmproj_bytes,
        gpu: Some(snap.gpu.clone()),
        calib: Calibration::default(),
    });
    let kvt = KvType::from_cache(kv).unwrap_or(KvType::F16);
    let need = plan.need_mib(ctx / np, kvt, np as u16);
    let dedicated: f64 = snap.runners.iter().map(|r| r.1).sum();
    let ps_mib = m.size_vram as f64 / 1048576.0;
    eprintln!("whole runner (MiB):");
    eprintln!("  log, every GPU buffer      {:>10.1}", load.gpu_mib());
    eprintln!("  predicted (uncalibrated)   {need:>10.1}   vs PDH {:+.2}%", (need - dedicated) / dedicated * 100.0);
    eprintln!("  PDH dedicated              {dedicated:>10.1}   (runners {:?})", snap.runners);
    eprintln!("  /api/ps size_vram          {ps_mib:>10.1}   vs PDH {:+.2}%", (ps_mib - dedicated) / dedicated * 100.0);
    eprintln!("  driver overhead measured   {:>10.1}", dedicated - load.gpu_mib());
    eprintln!("  NVML: used {:.0}, free {:.0}, others {:.0}, budget {:.0}, bandwidth {:?} GB/s", snap.gpu.used_mib, snap.gpu.free_mib, snap.gpu.others_mib(), snap.gpu.budget_mib(), snap.gpu.bandwidth_gbs);

    // And through the library, as the client sees it.
    let mut f = ProviderFactory::new(Arc::new(MemoryStore::new()));
    f.set_hardware(Hardware { models: Arc::new(hw), gpu: Arc::new(gpu), server: Arc::new(Refuse) });
    let lib = Library::open_in_memory().unwrap();
    lib.add(&ModelInfo::bare(ProviderKind::Ollama, &m.name)).unwrap();
    let b = Brains::new(lib, f);
    let r: ModelRef = format!("ollama/{}", m.name).parse().unwrap();
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
