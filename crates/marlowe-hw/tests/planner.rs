//! The planner against headers read from real GGUF files on Matthew's machine (2026-10-09) and
//! the runner log of the same load. The numbers asserted here are the runner's own.

use marlowe_hw::arch::{LayerRole, Role};
use marlowe_hw::plan::{GpuBudget, MIB};
use marlowe_hw::{build, calibrate, log, predict, ArchProfile, CacheType, Calibration, Fit, KvType, ModelHeader, PlanInputs};

fn fixture(name: &str) -> ModelHeader {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    ModelHeader::from_show(&v).unwrap()
}

fn text(name: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn dusk() -> (ArchProfile, ArchProfile) {
    let main = ArchProfile::analyze(&fixture("qwen35_dusk_27b_super.json"), Role::Main).unwrap();
    let draft = ArchProfile::analyze(&fixture("qwen35_dusk_draft_mtp.json"), Role::Draft).unwrap();
    (main, draft)
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn dusk_super_layers_kv_and_state_match_the_runner() {
    let (m, d) = dusk();
    assert_eq!((m.n_blocks, m.n_main), (65, 64));
    assert_eq!(m.kv_layers(), 16, "every fourth layer keeps a KV cache");
    assert_eq!(m.roles[3], LayerRole::Attention);
    assert_eq!(m.roles[0], LayerRole::Recurrent);
    assert_eq!(m.roles[64], LayerRole::Mtp);
    assert_eq!(m.kv_per_token(CacheType::F16, CacheType::F16), 65_536);
    assert_eq!(m.kv_per_token(CacheType::Q8_0, CacheType::Q8_0), 34_816);
    assert_eq!(m.kv_per_token(CacheType::Q4_0, CacheType::Q4_0), 18_432);
    let r = m.recurrent.as_ref().unwrap();
    assert_eq!((r.kind, r.layers), ("Gated DeltaNet", 48));
    // The log: R (f32) 5.63 MiB, S (f32) 144.00 MiB, 149.63 MiB per sequence.
    assert!(close(r.r_bytes as f64 / MIB, 5.625, 1e-9));
    assert!(close(r.s_bytes as f64 / MIB, 144.0, 1e-9));
    assert!(close(m.state_per_seq() as f64 / MIB, 149.625, 1e-9));
    // Weights: CUDA0 10019.96 MiB, CUDA_Host 397.85 MiB (token_embd).
    assert!(close(m.weights.gpu() as f64 / MIB, 10_019.96, 0.005), "{}", m.weights.gpu() as f64 / MIB);
    assert!(close(m.weights.host as f64 / MIB, 397.85, 0.005));
    assert_eq!(m.summary(), "64 layers, 16 keep a KV cache, 48 Gated DeltaNet, 1 MTP");
    // The draft: its MTP block is its only cache layer, 4 heads x (256 + 256) at f16.
    assert_eq!(d.kv_layers(), 1);
    assert_eq!(d.kv_per_token(CacheType::F16, CacheType::F16), 4_096);
    assert!(close(d.weights.gpu() as f64 / MIB, 774.71, 0.005));
    assert!(close(d.weights.host as f64 / MIB, 521.00, 0.005));
    assert_eq!(m.n_ctx_train, Some(262_144));
    assert_eq!(m.quant.as_deref(), Some("IQ3_XXS"));
}

#[test]
fn the_runner_log_parses_into_every_buffer() {
    let blocks = log::parse(&text("server_dusk_super.log"));
    let b = blocks.last().unwrap();
    let l = &b.launch;
    assert_eq!((l.ctx, l.parallel, l.spec_n, l.ubatch, l.port), (Some(32_768), Some(1), Some(3), Some(512), Some(61_916)));
    assert_eq!(l.cache_k.as_deref(), Some("q8_0"));
    assert!(l.draft.as_deref().unwrap().ends_with("sha256-50d9ce5a6da381bbcfb31061cf73df94a90e6faf8efeddee379a9cb8f1501c6e"));
    let main = b.main().unwrap();
    assert_eq!(main.offloaded, Some((66, 66)));
    assert_eq!(main.model.get("CUDA0"), Some(&10_019.96));
    assert_eq!(main.model.get("CUDA_Host"), Some(&397.85));
    assert_eq!(main.kv.get("CUDA0"), Some(&1_088.0));
    assert_eq!(main.rs.get("CUDA0"), Some(&598.5));
    assert_eq!(main.rs_line.as_ref().unwrap().rs_seq, 3);
    assert_eq!(main.compute.get("CUDA0"), Some(&240.28));
    let draft = b.draft().unwrap();
    assert_eq!(draft.model.get("CUDA0"), Some(&774.71));
    assert_eq!(draft.kv_lines[0].k_type, "f16");
    assert_eq!(draft.compute.get("CUDA0"), Some(&130.02));
    // Every GPU buffer of the load: the 12,979.47 MiB the spec's validation table sums.
    assert!(close(b.gpu_mib(), 10_019.96 + 1_088.0 + 598.5 + 240.28 + 774.71 + 128.0 + 130.02, 0.01), "{}", b.gpu_mib());
    assert_eq!(b.system_total_gib, Some(31.2));
    assert_eq!(b.started_secs, Some(6.84));
}

#[test]
fn a_partial_offload_is_visible_in_the_log() {
    let b = log::parse(&text("server_valiance_partial.log")).remove(0);
    let main = b.main().unwrap();
    assert_eq!(main.offloaded, Some((57, 65)));
    assert_eq!(main.kv.get("CPU"), Some(&136.0), "2 of 16 attention layers' KV on the CPU");
    assert_eq!(main.rs.get("CPU"), Some(&18.70), "6 of 48 recurrent layers' state on the CPU");
    assert_eq!(main.model.get("CUDA_Host"), Some(&2_420.50));
    assert!(b.fit_reduced_layers, "the fit step kept the context and moved layers");
}

#[test]
fn prediction_matches_the_runner_buffer_for_buffer() {
    let (m, d) = dusk();
    let p = predict(&m, Some(&d), 32_768, 1, CacheType::Q8_0, CacheType::F16, 3, 512);
    let b = log::parse(&text("server_dusk_super.log")).pop().unwrap();
    let (main, draft) = (b.main().unwrap(), b.draft().unwrap());
    assert!(close(p.main_gpu_weights, main.model["CUDA0"], 0.005));
    assert!(close(p.main_host_weights, main.model["CUDA_Host"], 0.005));
    assert!(close(p.main_kv, main.kv_total(), 0.005));
    assert!(close(p.main_state, main.rs_line.as_ref().unwrap().mib, 0.005), "x4 with the draft: {}", p.main_state);
    assert!(close(p.draft_gpu_weights, draft.model["CUDA0"], 0.005));
    assert!(close(p.draft_host_weights, draft.model["CUDA_Host"], 0.005));
    assert!(close(p.draft_kv, draft.kv_total(), 0.005));
}

#[test]
fn uncalibrated_total_is_within_half_a_percent_of_the_measured_runner() {
    // Before any load is measured: estimated compute (main + half for the draft) and 512 MiB
    // of driver. PDH measured the runner's dedicated memory at 13,485.7 MiB.
    let (m, d) = dusk();
    let plan = build(&PlanInputs {
        main: &m,
        draft: Some(&d),
        spec_n: 3,
        draft_kv: CacheType::F16,
        ubatch: 512,
        mmproj_b: 0,
        gpu: None,
        calib: Calibration::default(),
    });
    let need = plan.need_mib(32_768, KvType::Q8_0, 1);
    let err = (need - 13_485.7) / 13_485.7;
    assert!((0.0..0.005).contains(&err), "predicted {need:.1} MiB ({:+.2}%)", err * 100.0);
    // Calibrated from the log and PDH: the driver's share is what is left.
    let b = log::parse(&text("server_dusk_super.log")).pop().unwrap();
    let c = calibrate(&b, Some(13_485.7));
    assert!(close(c.driver_mib.unwrap(), 13_485.7 - 12_979.47, 0.01));
    assert_eq!(c.compute.map(|(ctx, _)| ctx), Some(32_768));
}

fn clean_desktop(m: &ArchProfile, d: Option<&ArchProfile>) -> marlowe_hw::PlanModel {
    let b = log::parse(&text("server_dusk_super.log")).pop().unwrap();
    let mut calib = calibrate(&b, Some(13_485.7));
    if d.is_none() {
        // Without the draft only the main context's compute was measured.
        calib.compute = Some((32_768, 240.28));
    }
    build(&PlanInputs {
        main: m,
        draft: d,
        spec_n: if d.is_some() { 3 } else { 0 },
        draft_kv: CacheType::F16,
        ubatch: 512,
        mmproj_b: 0,
        // Free at Ollama's start: 1.3 GiB used by other apps, a 14,037 MiB budget.
        gpu: Some(GpuBudget {
            name: "NVIDIA GeForce RTX 4080 SUPER".into(),
            total_mib: 16_375.0,
            used_mib: 1_314.0,
            free_mib: 15_061.0,
            runners_mib: 0.0,
            runners_shared_mib: 0.0,
            headroom_mib: 1_024.0,
            bandwidth_gbs: Some(736.0),
        }),
        calib,
    })
}

#[test]
fn agents_that_fit_on_the_clean_desktop_budget() {
    let (m, d) = dusk();
    let p = clean_desktop(&m, Some(&d));
    assert!(close(p.budget_mib().unwrap(), 14_037.0, 0.01));
    assert!(close(p.fixed_mib(32_768), 11_671.2, 0.1), "{}", p.fixed_mib(32_768));
    let table = |kv| [8_192, 16_384, 32_768, 65_536].map(|c| p.agents(c, kv).unwrap());
    assert_eq!(table(KvType::F16), [2, 1, 0, 0]);
    assert_eq!(table(KvType::Q8_0), [2, 1, 1, 0]);
    assert_eq!(table(KvType::Q4_0), [3, 2, 1, 1]);
    // Per agent at q8_0 with the draft: 0.9 GiB at 8k, 1.8 at 32k, 10.1 at 256k.
    assert!(close(p.per_agent_mib(8_192, KvType::Q8_0) / 1024.0, 0.88, 0.01));
    assert!(close(p.per_agent_mib(32_768, KvType::Q8_0) / 1024.0, 1.77, 0.01));
    assert!(close(p.per_agent_mib(262_144, KvType::Q8_0) / 1024.0, 10.08, 0.01));
    let single = |kv| p.max_ctx(kv, 1, 1024).unwrap();
    assert!((45_056..=48_128).contains(&single(KvType::Q8_0)), "{}", single(KvType::Q8_0));
    assert!((78_848..=82_944).contains(&single(KvType::Q4_0)), "{}", single(KvType::Q4_0));
    assert!(matches!(p.fit(262_144, KvType::Q8_0, 1), Fit::NoFit { .. }), "{:?}", p.fit(262_144, KvType::Q8_0, 1));
    match p.fit(131_072, KvType::Q8_0, 1) {
        Fit::Partial { gpu_layers, total_layers, slowdown } => {
            assert_eq!(total_layers, 66);
            assert!((40..66).contains(&gpu_layers), "{gpu_layers}");
            assert!((1.5..4.0).contains(&slowdown), "{slowdown}");
        }
        f => panic!("{f:?}"),
    }
    assert_eq!(p.auto_ctx(KvType::Q8_0, 1), Some(45_056));
    // The popup's "without the draft" line comes from the same model.
    let nd = p.without_draft();
    assert_eq!((nd.state_copies, nd.draft_kv_token_b), (1, 0));
    assert!((88_064..=96_256).contains(&nd.max_ctx(KvType::Q8_0, 1, 1024).unwrap()));
}

#[test]
fn without_the_draft_more_agents_fit() {
    let (m, _) = dusk();
    let p = clean_desktop(&m, None);
    assert_eq!(p.state_copies, 1);
    assert_eq!(p.agents(8_192, KvType::Q8_0), Some(7));
    assert_eq!(p.agents(32_768, KvType::Q8_0), Some(2));
    let single = p.max_ctx(KvType::Q8_0, 1, 1024).unwrap();
    assert!((91_136..=94_208).contains(&single), "{single}");
    assert_eq!(p.agents(8_192, KvType::Q4_0), Some(11));
}

#[test]
fn llama_full_attention_costs_128_kib_a_token_at_f16() {
    let m = ArchProfile::analyze(&fixture("llama3_8b_q4_0.json"), Role::Main).unwrap();
    assert_eq!(m.kv_layers(), 32);
    assert!(m.recurrent.is_none() && m.n_swa.is_none());
    assert_eq!(m.kv_per_token(CacheType::F16, CacheType::F16), 131_072);
    assert_eq!(m.kv_per_token(CacheType::Q8_0, CacheType::Q8_0), 69_632);
    // /api/show of the same model gives the same weights.
    let show = ArchProfile::analyze(&fixture("show_llama3_8b.json"), Role::Main).unwrap();
    assert_eq!(show.weights, m.weights);
    assert_eq!(show.kv_per_token(CacheType::F16, CacheType::F16), 131_072);
}

#[test]
fn gemma3_keeps_five_of_six_layers_in_a_1024_window() {
    let m = ArchProfile::analyze(&fixture("gemma3_12b_q4_k_m.json"), Role::Main).unwrap();
    assert_eq!(m.kv_layers(), 48);
    assert_eq!(m.swa_layers(), 40);
    assert_eq!(m.n_swa, Some(1024));
    assert_eq!(m.roles[5], LayerRole::Attention);
    assert_eq!(m.roles[4], LayerRole::SlidingWindow);
    // 8 global layers x 8 heads x (256 + 256) x 2 B.
    assert_eq!(m.kv_per_token(CacheType::F16, CacheType::F16), 65_536);
    // Each SWA layer keeps pad256(1024 + 512) = 1536 cells, whatever the context.
    assert_eq!(m.swa_cells(131_072, 512), 1_536);
    assert_eq!(m.swa_cells(1_024, 512), 1_024);
    assert_eq!(m.per_seq_bytes(32_768, CacheType::F16, CacheType::F16, 512, 1), 32_768 * 65_536 + 1_536 * 40 * 8_192);
    assert!(m.weights.vision > 0, "the vision tower is in the main file");
    assert!(m.weights.gpu_other >= m.weights.host, "tied embeddings: a GPU copy of token_embd");
}

#[test]
fn gpt_oss_alternates_sliding_and_full_layers() {
    let m = ArchProfile::analyze(&fixture("gptoss_20b_mxfp4.json"), Role::Main).unwrap();
    assert_eq!((m.kv_layers(), m.swa_layers()), (24, 12));
    assert_eq!(m.roles[0], LayerRole::SlidingWindow);
    assert_eq!(m.roles[1], LayerRole::Attention);
    assert_eq!(m.kv_per_token(CacheType::F16, CacheType::F16), 12 * 8 * 128 * 2);
    assert_eq!(m.swa_cells(32_768, 512), 768);
    assert_eq!(m.weights.unknown, 0, "MXFP4 is in the table");
}

#[test]
fn mla_keeps_one_compressed_head() {
    let m = ArchProfile::analyze(&fixture("deepseek2_lite_mla_constructed.json"), Role::Main).unwrap();
    assert!(m.mla);
    assert_eq!(m.kv_layers(), 27);
    assert_eq!(m.kv_per_token(CacheType::F16, CacheType::F16), 27 * (576 + 512) * 2);
}

#[test]
fn growing_the_context_never_lets_more_agents_fit() {
    let (m, d) = dusk();
    let p = clean_desktop(&m, Some(&d));
    for kv in KvType::ALL {
        let mut last = u16::MAX;
        for c in (2_048..=262_144).step_by(2_048) {
            let n = p.agents(c, kv).unwrap();
            assert!(n <= last, "{kv} at {c}: {n} > {last}");
            last = n;
            if let Some(max) = p.max_ctx(kv, n.max(1), 1024) {
                if n >= 1 {
                    assert!(max >= c / 1024 * 1024 || max + 1024 > c, "{kv}: {n} agents fit at {c} but max says {max}");
                }
            }
        }
    }
}

/// Reads the real blob headers from Ollama's store. Ignored by default (machine-specific).
#[test]
#[ignore]
fn live_gguf_header_matches_the_fixture() {
    let root = std::env::var("USERPROFILE").map(|h| std::path::PathBuf::from(h).join(".ollama/models/blobs")).unwrap();
    let blob = root.join("sha256-17c315b6e2fc165a94c52bc7ace9b8ef8fd599ded3dd5bd7dae5ce17479a4d15");
    let t = std::time::Instant::now();
    let h = marlowe_hw::gguf::read_header(&blob).unwrap();
    eprintln!("header read in {:?}", t.elapsed());
    let live = ArchProfile::analyze(&h, Role::Main).unwrap();
    let (fx, _) = dusk();
    assert_eq!(live.weights, fx.weights);
    assert_eq!(live.kv_per_token(CacheType::Q8_0, CacheType::Q8_0), 34_816);
}
