//! What a model's architecture costs per agent: which layers keep a KV cache, how many bytes a
//! token costs in it, the fixed recurrent state, and where the weights live.
//!
//! Layer roles, for each block below `n_main = block_count − nextn_predict_layers`:
//! 1. `{arch}.attention.recurrent_layers` (bools): true is recurrent;
//! 2. else `{arch}.full_attention_interval = k`: full attention when `(l+1) % k == 0`;
//! 3. else a per-layer `{arch}.attention.head_count_kv` of 0 is recurrent (when the layer has
//!    state tensors) or keeps no cache;
//! 4. else tensor presence: `ssm_*`, `time_mix_*` or `shortconv*` is recurrent.
//!
//! The last `nextn_predict_layers` blocks are MTP heads: no main KV (their KV belongs to the
//! draft context). Absent blocks (a draft GGUF holds only its MTP block) cost nothing.

use serde::{Deserialize, Serialize};

use crate::ggml::CacheType;
use crate::header::ModelHeader;

/// Which context the header is planned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The model itself.
    Main,
    /// A speculative DRAFT model (Ollama's `application/vnd.ollama.image.draft` layer): every
    /// block present in the file keeps its cache, MTP blocks included.
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerRole {
    /// Full attention: a KV cache that grows with the context.
    Attention,
    /// Sliding-window attention: a KV cache capped at the window.
    SlidingWindow,
    /// Linear attention / SSM / RWKV: a fixed state per sequence.
    Recurrent,
    /// A multi-token-prediction head: weights only in the main context.
    Mtp,
    /// No cache at all (shared KV, absent from the file).
    NoCache,
}

/// One cache-holding attention layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttnLayer {
    pub layer: u32,
    pub heads_kv: u64,
    pub k: u64,
    pub v: u64,
    pub swa: bool,
}

/// The fixed per-sequence state of the recurrent layers, f32.
#[derive(Debug, Clone, PartialEq)]
pub struct Recurrent {
    /// `Gated DeltaNet`, `Mamba-2`, `Mamba`, `RWKV`, `short conv`.
    pub kind: &'static str,
    pub layers: u32,
    /// Bytes per sequence, all recurrent layers: the conv/shift state R and the SSM state S.
    pub r_bytes: u64,
    pub s_bytes: u64,
}

impl Recurrent {
    pub fn bytes(&self) -> u64 {
        self.r_bytes + self.s_bytes
    }
}

/// Where the weights go. `host` stays in system RAM (the input embeddings); everything else is on
/// the GPU when fully offloaded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Weights {
    pub host: u64,
    /// Output head, norms, a tied-embedding copy: the "output layer" llama.cpp offloads last.
    pub gpu_other: u64,
    /// Per block (MTP blocks included).
    pub per_block: Vec<u64>,
    /// Vision (or audio) encoder and projector tensors in the main file.
    pub vision: u64,
    /// Tensors whose ggml type this table does not know (counted as 0 bytes).
    pub unknown: u32,
}

impl Weights {
    /// Bytes on the GPU when every layer is offloaded (vision excluded).
    pub fn gpu(&self) -> u64 {
        self.gpu_other + self.per_block.iter().sum::<u64>()
    }
}

/// The planner's view of one model file.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchProfile {
    pub arch: String,
    pub role: Role,
    pub n_blocks: u32,
    pub n_main: u32,
    pub roles: Vec<LayerRole>,
    pub attn: Vec<AttnLayer>,
    /// The sliding window, when some layers use one.
    pub n_swa: Option<u64>,
    /// Multi-head latent attention: one compressed head per layer.
    pub mla: bool,
    pub recurrent: Option<Recurrent>,
    pub weights: Weights,
    pub n_embd: u64,
    pub n_ctx_train: Option<u32>,
    pub params: Option<u64>,
    pub quant: Option<String>,
    pub warnings: Vec<String>,
}

/// Architectures whose every layer is recurrent.
const PURE_RECURRENT: &[&str] = &["mamba", "mamba2", "rwkv6", "rwkv6qwen2", "rwkv7", "arwkv7"];

/// Sliding-window periods llama.cpp hard-codes when the GGUF carries no pattern.
fn default_swa_period(arch: &str) -> Option<u32> {
    match arch {
        "gemma2" | "gptoss" => Some(2),
        "gemma3" => Some(6),
        "gemma3n" => Some(5),
        "cohere2" | "exaone4" => Some(4),
        _ => None,
    }
}

fn is_state_tensor(suffix: &str) -> bool {
    suffix.starts_with("ssm_") || suffix.starts_with("time_mix_") || suffix.starts_with("shortconv")
}

fn is_attn_tensor(suffix: &str) -> bool {
    ["attn_k.", "attn_k_b.", "attn_kv_a_mqa.", "attn_q.", "attn_v."].iter().any(|p| suffix.starts_with(p))
}

/// `llama_ftype` names, for the header's `general.file_type`.
pub fn file_type_name(id: u64) -> Option<&'static str> {
    Some(match id {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4",
        _ => return None,
    })
}

/// Pads up to llama.cpp's 256-cell granularity.
pub fn pad256(n: u64) -> u64 {
    n.div_ceil(256) * 256
}

impl ArchProfile {
    /// Analyses `h` for `role`.
    pub fn analyze(h: &ModelHeader, role: Role) -> Result<ArchProfile, String> {
        let arch = h.arch().ok_or("the header names no architecture")?.to_string();
        let mut warnings = Vec::new();
        let n_blocks = h.a_u64("block_count").ok_or_else(|| format!("{arch}.block_count is missing"))? as u32;
        let nextn = h.a_u64("nextn_predict_layers").unwrap_or(0) as u32;
        let n_main = n_blocks.saturating_sub(nextn);
        let n_embd = h.a_u64("embedding_length").unwrap_or(0);
        let has_tensors = !h.tensors.is_empty();

        // Which blocks are in the file, and which carry state or attention tensors.
        let mut present = vec![!has_tensors; n_blocks as usize];
        let mut state = vec![false; n_blocks as usize];
        let mut attn_t = vec![false; n_blocks as usize];
        for t in &h.tensors {
            if let Some(l) = t.layer().filter(|l| *l < n_blocks) {
                let suffix = t.name.splitn(3, '.').nth(2).unwrap_or("");
                present[l as usize] = true;
                state[l as usize] |= is_state_tensor(suffix);
                attn_t[l as usize] |= is_attn_tensor(suffix);
            }
        }

        let recurrent_flags = match h.a("attention.recurrent_layers") {
            Some(serde_json::Value::Array(a)) => Some(a.iter().map(|v| v.as_bool().unwrap_or(false)).collect::<Vec<_>>()),
            _ => None,
        };
        let interval = h.a_u64("full_attention_interval").filter(|k| *k > 0);
        let pure = PURE_RECURRENT.contains(&arch.as_str());
        let shared_kv = h.a_u64("attention.shared_kv_layers").unwrap_or(0) as u32;

        // Sliding window.
        let n_swa = h.a_u64("attention.sliding_window").filter(|w| *w > 0);
        let swa_flags: Option<Vec<bool>> = match (n_swa, h.a("attention.sliding_window_pattern")) {
            (None, _) => None,
            (Some(_), Some(serde_json::Value::Array(a))) => Some(a.iter().map(|v| v.as_bool().unwrap_or(false)).collect()),
            (Some(_), Some(v)) if v.as_u64().is_some() => {
                let p = v.as_u64().unwrap_or(0) as u32;
                Some((0..n_blocks).map(|l| p == 0 || l % p < p - 1).collect())
            }
            (Some(_), _) => match default_swa_period(&arch) {
                Some(p) => Some((0..n_blocks).map(|l| l % p < p - 1).collect()),
                None => {
                    warnings.push(format!(
                        "{arch} has a sliding window but no layer pattern; planned as full attention (an over-estimate)"
                    ));
                    None
                }
            },
        };

        // MLA: one compressed head of kv_lora_rank + rope dims for K, kv_lora_rank for V.
        let mla_rank = h.a_u64("attention.kv_lora_rank").filter(|r| *r > 0);
        let n_rot = h.a_u64("rope.dimension_count").unwrap_or(64);

        let mut roles = Vec::with_capacity(n_blocks as usize);
        let mut attn = Vec::new();
        let mut n_recurrent = 0u32;
        for l in 0..n_blocks {
            let li = l as usize;
            let role_here = if !present[li] {
                LayerRole::NoCache
            } else if role == Role::Main && l >= n_main {
                LayerRole::Mtp
            } else if role == Role::Draft {
                if state[li] {
                    LayerRole::Recurrent
                } else if attn_t[li] || !has_tensors {
                    LayerRole::Attention
                } else {
                    LayerRole::NoCache
                }
            } else {
                let heads_kv = h.a_layer("attention.head_count_kv", l);
                let recurrent = if let Some(f) = &recurrent_flags {
                    f.get(li).copied().unwrap_or(false)
                } else if let Some(k) = interval {
                    (u64::from(l) + 1) % k != 0
                } else {
                    // A per-layer `head_count_kv` of 0 is recurrent when the layer has state
                    // tensors; otherwise it keeps no cache (below).
                    state[li] || pure
                };
                if recurrent {
                    LayerRole::Recurrent
                } else if heads_kv == Some(0) || (shared_kv > 0 && l >= n_main.saturating_sub(shared_kv)) {
                    LayerRole::NoCache
                } else {
                    LayerRole::Attention
                }
            };
            let role_here = match role_here {
                LayerRole::Attention if swa_flags.as_ref().is_some_and(|f| f.get(li).copied().unwrap_or(false)) => {
                    LayerRole::SlidingWindow
                }
                r => r,
            };
            match role_here {
                LayerRole::Attention | LayerRole::SlidingWindow => {
                    let (heads_kv, k, v) = match mla_rank {
                        Some(rank) => (1, rank + n_rot, rank),
                        None => {
                            let n_head = h.a_layer("attention.head_count", l).filter(|n| *n > 0).unwrap_or(1);
                            let heads_kv = h.a_layer("attention.head_count_kv", l).filter(|n| *n > 0).unwrap_or(n_head);
                            let head_dim = n_embd / n_head.max(1);
                            let k = h.a_u64("attention.key_length").unwrap_or(head_dim);
                            let v = h.a_u64("attention.value_length").unwrap_or(head_dim);
                            (heads_kv, k, v)
                        }
                    };
                    attn.push(AttnLayer { layer: l, heads_kv, k, v, swa: role_here == LayerRole::SlidingWindow });
                }
                LayerRole::Recurrent => n_recurrent += 1,
                _ => {}
            }
            roles.push(role_here);
        }

        let recurrent = (n_recurrent > 0).then(|| recurrent_state(h, &arch, n_embd, n_recurrent, &mut warnings));

        // Weights.
        let mut w = Weights { per_block: vec![0; n_blocks as usize], ..Weights::default() };
        let tied = h.tensor("output.weight").is_none();
        for t in &h.tensors {
            let b = t.bytes();
            if t.ty.is_none() {
                w.unknown += 1;
            }
            if t.name == "token_embd.weight" {
                w.host += b;
                if tied {
                    w.gpu_other += b;
                }
            } else if let Some(l) = t.layer() {
                if (l as usize) >= w.per_block.len() {
                    w.per_block.resize(l as usize + 1, 0);
                }
                w.per_block[l as usize] += b;
            } else if ["v.", "mm.", "vision.", "mmproj.", "a.", "audio."].iter().any(|p| t.name.starts_with(p)) {
                w.vision += b;
            } else {
                w.gpu_other += b;
            }
        }
        if w.unknown > 0 {
            warnings.push(format!("{} tensors have a type this planner does not know; their bytes are not counted", w.unknown));
        }

        Ok(ArchProfile {
            n_ctx_train: h.a_u64("context_length").map(|c| c.min(u64::from(u32::MAX)) as u32),
            // Ollama's /api/show carries the count; a bare GGUF header may not, so count elements.
            params: h.general_u64("parameter_count").or_else(|| {
                let n: u64 = h.tensors.iter().map(|t| t.shape.iter().product::<u64>()).sum();
                (n > 0).then_some(n)
            }),
            quant: h.general_u64("file_type").and_then(file_type_name).map(str::to_string),
            arch,
            role,
            n_blocks,
            n_main,
            roles,
            attn,
            n_swa: if swa_flags.is_some() { n_swa } else { None },
            mla: mla_rank.is_some(),
            recurrent,
            weights: w,
            n_embd,
            warnings,
        })
    }

    /// Bytes per token per sequence in the full-attention layers.
    pub fn kv_per_token(&self, k: CacheType, v: CacheType) -> u64 {
        self.attn.iter().filter(|a| !a.swa).map(|a| a.heads_kv * (k.bytes_for(a.k) + v.bytes_for(a.v))).sum()
    }

    /// Bytes per cell in the sliding-window layers.
    pub fn swa_per_cell(&self, k: CacheType, v: CacheType) -> u64 {
        self.attn.iter().filter(|a| a.swa).map(|a| a.heads_kv * (k.bytes_for(a.k) + v.bytes_for(a.v))).sum()
    }

    /// Cells a sliding-window layer keeps per sequence (non-unified cache, no `--swa-full`).
    pub fn swa_cells(&self, ctx_seq: u64, ubatch: u64) -> u64 {
        match self.n_swa {
            Some(w) => ctx_seq.min(pad256(w + ubatch)),
            None => 0,
        }
    }

    /// The recurrent state per sequence (no speculative copies).
    pub fn state_per_seq(&self) -> u64 {
        self.recurrent.as_ref().map_or(0, Recurrent::bytes)
    }

    /// Layers that keep a KV cache (full and sliding).
    pub fn kv_layers(&self) -> usize {
        self.attn.len()
    }

    pub fn swa_layers(&self) -> usize {
        self.attn.iter().filter(|a| a.swa).count()
    }

    /// Cache bytes one sequence of `ctx` tokens costs: KV, sliding-window cells, and the
    /// recurrent state times `state_copies` (1 + draft positions under speculative decoding).
    pub fn per_seq_bytes(&self, ctx: u64, k: CacheType, v: CacheType, ubatch: u64, state_copies: u64) -> u64 {
        let ctx = pad256(ctx);
        ctx * self.kv_per_token(k, v) + self.swa_cells(ctx, ubatch) * self.swa_per_cell(k, v) + self.state_per_seq() * state_copies
    }

    /// Cache bytes of block `l` for one sequence (for partial offload).
    pub fn layer_cache_bytes(&self, l: u32, ctx: u64, k: CacheType, v: CacheType, ubatch: u64, state_copies: u64) -> u64 {
        let ctx = pad256(ctx);
        match self.roles.get(l as usize) {
            Some(LayerRole::Attention | LayerRole::SlidingWindow) => {
                let Some(a) = self.attn.iter().find(|a| a.layer == l) else { return 0 };
                let cells = if a.swa { self.swa_cells(ctx, ubatch) } else { ctx };
                cells * a.heads_kv * (k.bytes_for(a.k) + v.bytes_for(a.v))
            }
            Some(LayerRole::Recurrent) => {
                let n = self.recurrent.as_ref().map_or(1, |r| r.layers.max(1));
                self.state_per_seq() / u64::from(n) * state_copies
            }
            _ => 0,
        }
    }

    /// Smallest head dimension among the cache layers (quantized V needs a multiple of 32).
    pub fn quantizable(&self) -> bool {
        self.attn.iter().all(|a| a.k % 32 == 0 && a.v % 32 == 0)
    }

    /// `16 keep a KV cache, 48 Gated DeltaNet`.
    pub fn summary(&self) -> String {
        let main = self.roles.iter().filter(|r| !matches!(r, LayerRole::Mtp | LayerRole::NoCache)).count();
        let mut parts = vec![format!("{} layers", main)];
        let full = self.attn.iter().filter(|a| !a.swa).count();
        if full > 0 {
            parts.push(format!("{full} keep a KV cache{}", if self.mla { " (MLA)" } else { "" }));
        }
        if let Some(w) = self.n_swa.filter(|_| self.swa_layers() > 0) {
            parts.push(format!("{} sliding window {}", self.swa_layers(), w));
        }
        if let Some(r) = &self.recurrent {
            parts.push(format!("{} {}", r.layers, r.kind));
        }
        let mtp = self.roles.iter().filter(|r| **r == LayerRole::Mtp).count();
        if mtp > 0 {
            parts.push(format!("{mtp} MTP"));
        }
        parts.join(", ")
    }
}

fn recurrent_state(h: &ModelHeader, arch: &str, n_embd: u64, layers: u32, warnings: &mut Vec<String>) -> Recurrent {
    let n = u64::from(layers);
    if let (Some(d_conv), Some(d_inner), Some(d_state)) =
        (h.a_u64("ssm.conv_kernel"), h.a_u64("ssm.inner_size"), h.a_u64("ssm.state_size"))
    {
        let n_group = h.a_u64("ssm.group_count").unwrap_or(0);
        let kind = match arch {
            "qwen3next" | "qwen35" | "qwen35moe" => "Gated DeltaNet",
            _ if n_group > 0 => "Mamba-2",
            _ => "Mamba",
        };
        let r = d_conv.saturating_sub(1) * (d_inner + 2 * n_group * d_state) * 4;
        let s = d_state * d_inner * 4;
        return Recurrent { kind, layers, r_bytes: r * n, s_bytes: s * n };
    }
    if let Some(head) = h.a_u64("wkv.head_size") {
        let shift = h.a_u64("token_shift_count").unwrap_or(2);
        return Recurrent { kind: "RWKV", layers, r_bytes: shift * n_embd * 4 * n, s_bytes: n_embd * head * 4 * n };
    }
    if let Some(l_cache) = h.a_u64("shortconv.l_cache") {
        return Recurrent { kind: "short conv", layers, r_bytes: n_embd * l_cache.saturating_sub(1) * 4 * n, s_bytes: 0 };
    }
    warnings.push(format!("{layers} recurrent layers with no state dimensions in the header; their state is not counted"));
    Recurrent { kind: "recurrent", layers, r_bytes: 0, s_bytes: 0 }
}
