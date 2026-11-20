//! Each part of a model sized against the memory it lives in: the GPU part against the budget,
//! the input layer (token and n-gram embedding tables) on disk when memory-mapped or in RAM when
//! not, experts moved to the CPU in RAM.
//!
//! `fixtures/flashnext/` is the header (metadata and tensor table, no weights; the tokenizer arrays
//! dropped) of unsloth/Qwen3.8-Flash-Next-GGUF UD-Q4_K_XL, all four shards, read with HTTP range
//! requests on 2026-11-20. Its 51B-parameter n-gram table (`per_layer_token_embd`, 26.8 GiB) is
//! the whole fourth shard.

use marlowe_hw::arch::Role;
use marlowe_hw::gguf::{read_header, write_header};
use marlowe_hw::plan::{GpuBudget, MIB};
use marlowe_hw::{build, ArchProfile, CacheType, Calibration, Experts, Fit, HostTables, KvType, Placement, PlanInputs, PlanModel};
use serde_json::json;

const GIB: f64 = 1024.0;

fn gpu(name: &str, total_gib: f64, free_gib: f64) -> GpuBudget {
    GpuBudget {
        name: name.into(),
        total_mib: total_gib * GIB,
        used_mib: (total_gib - free_gib) * GIB,
        free_mib: free_gib * GIB,
        runners_mib: 0.0,
        runners_shared_mib: 0.0,
        headroom_mib: 1024.0,
        bandwidth_gbs: Some(736.0),
    }
}

/// A 16 GiB desktop card with 1.3 GiB used by other apps.
fn desktop() -> GpuBudget {
    gpu("NVIDIA GeForce RTX 4080 SUPER", 16.0, 14.7)
}

/// GB10: 128 GiB of unified memory, about 110 GiB of it free for the engine.
fn gb10() -> GpuBudget {
    gpu("NVIDIA GB10", 128.0, 110.0)
}

fn plan(m: &ArchProfile, g: GpuBudget, placement: Placement) -> PlanModel {
    build(&PlanInputs {
        main: m,
        draft: None,
        spec_n: 0,
        draft_kv: CacheType::F16,
        ubatch: 512,
        mmproj_b: 0,
        gpu: Some(g),
        calib: Calibration::default(),
        placement,
    })
}

fn flash_next() -> ArchProfile {
    let first = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/flashnext/Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf");
    let h = read_header(&first).unwrap();
    assert_eq!(h.tensors.len(), 1224, "the first shard brings the tensors of all four");
    ArchProfile::analyze(&h, Role::Main).unwrap()
}

fn gib(mib: f64) -> f64 {
    (mib / GIB * 10.0).round() / 10.0
}

const DISK: Placement = Placement { host: HostTables::Disk, experts: Experts::Auto };
const RAM: Placement = Placement { host: HostTables::Ram, experts: Experts::Auto };

#[test]
fn flash_next_keeps_its_ngram_table_off_the_gpu_and_its_experts_move_first() {
    let m = flash_next();
    assert_eq!((m.arch.as_str(), m.n_blocks, m.n_expert, m.n_expert_used), ("qwen4exp", 48, 512, 10));
    let w = &m.weights;
    // The n-gram table and the token embeddings are the input layer: never on the GPU.
    assert_eq!(gib(w.host as f64 / MIB), 27.5);
    assert_eq!(w.expert_blocks(), 48);
    assert_eq!(gib(w.experts_first(48) as f64 / MIB), 71.7);
    assert_eq!(gib(w.gpu() as f64 / MIB), 76.2, "attention, shared experts, routed experts, output");

    // Before: every tensor was sized against VRAM (the n-gram table with the output layer).
    assert_eq!(gib((w.gpu() + w.host - token_embd()) as f64 / MIB), 103.0);

    // GB10, one agent at 256k q8_0: whole on the GPU, the n-gram table on disk.
    let p = plan(&m, gb10(), DISK);
    let s = p.split(262_144, KvType::Q8_0, 1);
    assert_eq!((s.cpu_moe, s.cpu_layers), (0, 0));
    assert!(matches!(p.fit(262_144, KvType::Q8_0, 1), Fit::Fits { .. }), "{:?}", p.fit(262_144, KvType::Q8_0, 1));
    assert_eq!(gib(s.disk_mib), 27.5);
    assert_eq!(gib(s.ram_mib), 11.6, "host compute, checkpoints and the prompt-cache cap");
    // Loaded into RAM instead: the same GPU part, the table resident.
    let r = plan(&m, gb10(), RAM).split(262_144, KvType::Q8_0, 1);
    assert_eq!((gib(r.gpu_mib), gib(r.disk_mib), gib(r.ram_mib)), (gib(s.gpu_mib), 0.0, 39.0));
    assert_eq!(gib(s.gpu_mib), 80.4);

    // The 16 GiB desktop: the experts of the first blocks go to the CPU, nothing else does.
    let p = plan(&m, desktop(), DISK);
    match p.fit(32_768, KvType::Q8_0, 1) {
        Fit::Experts { cpu_blocks, expert_blocks, slowdown, .. } => {
            assert_eq!(expert_blocks, 48);
            assert_eq!(cpu_blocks, 43);
            assert!((3.0..4.0).contains(&slowdown), "{slowdown}");
        }
        f => panic!("{f:?}"),
    }
    let s = p.split(32_768, KvType::Q8_0, 1);
    assert!(s.gpu_mib <= p.budget_mib().unwrap());
    assert_eq!(s.cpu_layers, 0, "attention, the shared layers and the KV cache stay on the GPU");
    assert_eq!(gib(s.disk_mib), 27.5);
    assert_eq!((gib(s.gpu_mib), gib(s.ram_mib)), (13.5, 75.5), "the experts of 43 blocks are in RAM");
    // Before: 104.2 GiB against a 13.7 GiB budget, a layer split keeping 8 of 49 layers.
    let old = p.with_placement(Placement { experts: Experts::Gpu, host: HostTables::Ram });
    assert_eq!(gib(old.whole_mib(32_768, KvType::Q8_0, 1) + (w.host - token_embd()) as f64 / MIB), 104.2);
}

/// Flash-Next's token embeddings: 2560 x 248320 at Q8_0.
fn token_embd() -> u64 {
    2560 / 32 * 34 * 248_320
}

/// A small model header: `blocks` blocks of attention and FFN, optionally MoE, optionally with an
/// n-gram table. Q8_0 everywhere (34 B per 32 weights).
fn model(blocks: u32, experts: u32, ngram_rows: u64) -> ArchProfile {
    let arch = "testarch";
    let kv = vec![
        ("general.architecture", json!(arch)),
        ("testarch.block_count", json!(blocks)),
        ("testarch.embedding_length", json!(2048)),
        ("testarch.attention.head_count", json!(16)),
        ("testarch.attention.head_count_kv", json!(4)),
        ("testarch.context_length", json!(131_072)),
        ("testarch.expert_count", json!(experts)),
        ("testarch.expert_used_count", json!(if experts > 0 { 4 } else { 0 })),
    ];
    let mut names: Vec<(String, u32, Vec<u64>)> = vec![
        ("token_embd.weight".into(), 8, vec![2048, 150_000]),
        ("output.weight".into(), 8, vec![2048, 150_000]),
        ("output_norm.weight".into(), 0, vec![2048]),
    ];
    if ngram_rows > 0 {
        names.push(("per_layer_token_embd.weight".into(), 8, vec![256, ngram_rows]));
    }
    for b in 0..blocks {
        for (t, shape) in [("attn_q", vec![2048, 2048]), ("attn_k", vec![2048, 512]), ("attn_v", vec![2048, 512]), ("attn_output", vec![2048, 2048])] {
            names.push((format!("blk.{b}.{t}.weight"), 8, shape));
        }
        if experts > 0 {
            names.push((format!("blk.{b}.ffn_gate_inp.weight"), 0, vec![2048, u64::from(experts)]));
            for t in ["ffn_gate_exps", "ffn_up_exps"] {
                names.push((format!("blk.{b}.{t}.weight"), 8, vec![2048, 768, u64::from(experts)]));
            }
            names.push((format!("blk.{b}.ffn_down_exps.weight"), 8, vec![768, 2048, u64::from(experts)]));
        } else {
            for t in ["ffn_gate", "ffn_up"] {
                names.push((format!("blk.{b}.{t}.weight"), 8, vec![2048, 8192]));
            }
            names.push((format!("blk.{b}.ffn_down.weight"), 8, vec![8192, 2048]));
        }
    }
    let tensors: Vec<(&str, u32, Vec<u64>)> = names.iter().map(|(n, t, s)| (n.as_str(), *t, s.clone())).collect();
    let bytes = write_header(&kv, &tensors);
    ArchProfile::analyze(&marlowe_hw::gguf::parse_header(&bytes[..]).unwrap(), Role::Main).unwrap()
}

#[test]
fn a_dense_model_puts_its_token_embeddings_on_disk_and_the_rest_on_the_gpu() {
    let m = model(24, 0, 0);
    assert_eq!(m.weights.expert_blocks(), 0);
    let p = plan(&m, desktop(), DISK);
    assert!(!p.has_experts());
    let s = p.split(32_768, KvType::Q8_0, 1);
    assert!(matches!(p.fit(32_768, KvType::Q8_0, 1), Fit::Fits { .. }));
    assert_eq!(s.gpu_mib, p.need_mib(32_768, KvType::Q8_0, 1));
    assert_eq!(s.disk_mib, m.weights.host as f64 / MIB);
    assert_eq!((s.cpu_moe, s.cpu_layers), (0, 0));
    // Not mapped: the embeddings are resident instead, the GPU part does not change.
    let r = plan(&m, desktop(), RAM).split(32_768, KvType::Q8_0, 1);
    assert_eq!((r.gpu_mib, r.disk_mib), (s.gpu_mib, 0.0));
    assert!((r.ram_mib - s.ram_mib - s.disk_mib).abs() < 1e-6);
}

#[test]
fn an_moe_that_does_not_fit_moves_experts_before_layers() {
    // 48 blocks of 64 experts: 14.9 GiB of experts, about 16.9 GiB on the GPU in all.
    let m = model(48, 64, 0);
    assert_eq!(m.weights.expert_blocks(), 48);
    let p = plan(&m, desktop(), DISK);
    let whole = p.whole_mib(32_768, KvType::Q8_0, 1);
    assert!(whole > p.budget_mib().unwrap(), "{whole}");
    let n = p.cpu_moe(32_768, KvType::Q8_0, 1);
    assert!(n > 0 && n < 48, "{n}");
    // The fewest first blocks: one fewer would not fit.
    assert!(whole - p.experts_first_mib(n) <= p.budget_mib().unwrap());
    assert!(whole - p.experts_first_mib(n - 1) > p.budget_mib().unwrap());
    let s = p.split(32_768, KvType::Q8_0, 1);
    assert_eq!((s.cpu_moe, s.cpu_layers), (n, 0));
    assert!((s.ram_mib - (p.host_fixed_mib + p.experts_first_mib(n) + 8192.0)).abs() < 1e-6, "experts in RAM");
    assert!(matches!(p.fit(32_768, KvType::Q8_0, 1), Fit::Experts { cpu_blocks, .. } if cpu_blocks == n));
    // Every expert on the CPU: the most room for context.
    let all = p.with_placement(Placement { experts: Experts::Cpu, ..DISK });
    assert_eq!(all.cpu_moe(8_192, KvType::Q8_0, 1), 48);
    assert!(all.max_ctx(KvType::Q8_0, 1, 1024).unwrap() >= p.with_placement(Placement { experts: Experts::Gpu, ..DISK }).max_ctx(KvType::Q8_0, 1, 1024).unwrap());
    // Experts kept on the GPU: the old layer split.
    let gpu = p.with_placement(Placement { experts: Experts::Gpu, ..DISK });
    assert_eq!(gpu.cpu_moe(32_768, KvType::Q8_0, 1), 0);
    assert!(matches!(gpu.fit(32_768, KvType::Q8_0, 1), Fit::Partial { .. } | Fit::NoFit { .. }));
    assert!(gpu.split(32_768, KvType::Q8_0, 1).cpu_layers > 0);
    // A model that fits moves nothing.
    let small = model(8, 64, 0);
    assert_eq!(plan(&small, desktop(), DISK).cpu_moe(32_768, KvType::Q8_0, 1), 0);
}

#[test]
fn a_large_ngram_table_costs_disk_not_vram() {
    // A 4 GiB n-gram table beside a 1.3 GiB dense model.
    let rows = 4 * 1024 * 1024 * 1024 / (256 / 32 * 34);
    let m = model(8, 0, rows);
    let table = (256 / 32 * 34 * rows) as f64 / MIB;
    assert_eq!(m.weights.host as f64 / MIB, table + (2048 / 32 * 34 * 150_000) as f64 / MIB);
    let p = plan(&m, desktop(), DISK);
    assert!(p.weights_main_mib < 2.0 * GIB, "the table is not GPU weight: {}", p.weights_main_mib);
    let s = p.split(32_768, KvType::Q8_0, 1);
    assert!(s.disk_mib >= table && s.ram_mib < 8.5 * GIB);
    let r = p.with_placement(RAM).split(32_768, KvType::Q8_0, 1);
    assert!(r.disk_mib == 0.0 && r.ram_mib >= s.ram_mib + table);
    assert_eq!(r.gpu_mib, s.gpu_mib);
}

#[test]
fn expert_and_input_tensors_are_named_as_llama_cpp_matches_them() {
    use marlowe_hw::arch::{is_expert_tensor, is_input_tensor};
    for n in ["blk.0.ffn_up_exps.weight", "blk.12.ffn_down_exps.weight", "blk.3.ffn_gate_up_exps.weight", "blk.3.ffn_gate_chexps.weight", "blk.1.ffn_up_exps.bias"] {
        assert!(is_expert_tensor(n), "{n}");
    }
    for n in ["blk.0.ffn_up_shexp.weight", "blk.0.ffn_gate_inp.weight", "blk.0.ffn_up.weight", "token_embd.weight"] {
        assert!(!is_expert_tensor(n), "{n}");
    }
    for n in ["token_embd.weight", "per_layer_token_embd.weight", "position_embd.weight", "token_types.weight"] {
        assert!(is_input_tensor(n), "{n}");
    }
    for n in ["token_embd_norm.weight", "output.weight", "blk.1.ple_key.weight", "per_layer_model_proj.weight"] {
        assert!(!is_input_tensor(n), "{n}");
    }
}
