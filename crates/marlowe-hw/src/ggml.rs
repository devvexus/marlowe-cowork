//! The ggml tensor and KV-cache types: block sizes and bytes per block.

use serde::{Deserialize, Serialize};

/// One ggml tensor type: `block` elements are stored in `bytes` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GgmlType {
    pub id: u32,
    pub name: &'static str,
    pub block: u32,
    pub bytes: u32,
}

const fn t(id: u32, name: &'static str, block: u32, bytes: u32) -> GgmlType {
    GgmlType { id, name, block, bytes }
}

/// ggml's type table (ggml.h `enum ggml_type`), ids as stored in a GGUF.
pub const TYPES: &[GgmlType] = &[
    t(0, "F32", 1, 4),
    t(1, "F16", 1, 2),
    t(2, "Q4_0", 32, 18),
    t(3, "Q4_1", 32, 20),
    t(6, "Q5_0", 32, 22),
    t(7, "Q5_1", 32, 24),
    t(8, "Q8_0", 32, 34),
    t(9, "Q8_1", 32, 36),
    t(10, "Q2_K", 256, 84),
    t(11, "Q3_K", 256, 110),
    t(12, "Q4_K", 256, 144),
    t(13, "Q5_K", 256, 176),
    t(14, "Q6_K", 256, 210),
    t(15, "Q8_K", 256, 292),
    t(16, "IQ2_XXS", 256, 66),
    t(17, "IQ2_XS", 256, 74),
    t(18, "IQ3_XXS", 256, 98),
    t(19, "IQ1_S", 256, 50),
    t(20, "IQ4_NL", 32, 18),
    t(21, "IQ3_S", 256, 110),
    t(22, "IQ2_S", 256, 82),
    t(23, "IQ4_XS", 256, 136),
    t(24, "I8", 1, 1),
    t(25, "I16", 1, 2),
    t(26, "I32", 1, 4),
    t(27, "I64", 1, 8),
    t(28, "F64", 1, 8),
    t(29, "IQ1_M", 256, 56),
    t(30, "BF16", 1, 2),
    t(34, "TQ1_0", 256, 54),
    t(35, "TQ2_0", 256, 66),
    t(39, "MXFP4", 32, 17),
];

impl GgmlType {
    pub fn from_id(id: u32) -> Option<GgmlType> {
        TYPES.iter().copied().find(|t| t.id == id)
    }

    /// By name as `/api/show` and llama.cpp print it (`Q4_K`, `IQ3_XXS`, `BF16`), any case.
    pub fn from_name(name: &str) -> Option<GgmlType> {
        let n = name.trim();
        TYPES.iter().copied().find(|t| t.name.eq_ignore_ascii_case(n))
    }

    /// Bytes for a tensor of `shape` (`shape[0]` is the row, as in ggml): whole blocks per row.
    pub fn tensor_bytes(&self, shape: &[u64]) -> u64 {
        let Some((&row, rest)) = shape.split_first() else { return 0 };
        let block = u64::from(self.block);
        let row_bytes = row.div_ceil(block) * u64::from(self.bytes);
        rest.iter().fold(row_bytes, |acc, &d| acc.saturating_mul(d))
    }
}

/// A KV-cache element type, as llama-server's `--cache-type-k/-v` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheType {
    F32,
    F16,
    Bf16,
    Q8_0,
    Q4_0,
    Q4_1,
    Q5_0,
    Q5_1,
    Iq4Nl,
}

impl CacheType {
    pub const ALL: [CacheType; 9] = [
        CacheType::F32,
        CacheType::F16,
        CacheType::Bf16,
        CacheType::Q8_0,
        CacheType::Q4_0,
        CacheType::Q4_1,
        CacheType::Q5_0,
        CacheType::Q5_1,
        CacheType::Iq4Nl,
    ];

    /// Bytes for 32 elements.
    pub fn bytes_per_32(self) -> u64 {
        match self {
            CacheType::F32 => 128,
            CacheType::F16 | CacheType::Bf16 => 64,
            CacheType::Q8_0 => 34,
            CacheType::Q4_0 | CacheType::Iq4Nl => 18,
            CacheType::Q4_1 => 20,
            CacheType::Q5_0 => 22,
            CacheType::Q5_1 => 24,
        }
    }

    /// Bytes for `n` elements (a head dimension not a multiple of 32 rounds up a block).
    pub fn bytes_for(self, n: u64) -> u64 {
        match self {
            CacheType::F32 => n * 4,
            CacheType::F16 | CacheType::Bf16 => n * 2,
            q => n.div_ceil(32) * q.bytes_per_32(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CacheType::F32 => "f32",
            CacheType::F16 => "f16",
            CacheType::Bf16 => "bf16",
            CacheType::Q8_0 => "q8_0",
            CacheType::Q4_0 => "q4_0",
            CacheType::Q4_1 => "q4_1",
            CacheType::Q5_0 => "q5_0",
            CacheType::Q5_1 => "q5_1",
            CacheType::Iq4Nl => "iq4_nl",
        }
    }

    pub fn parse(s: &str) -> Option<CacheType> {
        let s = s.trim();
        CacheType::ALL.into_iter().find(|t| t.name().eq_ignore_ascii_case(s))
    }

    /// Quantized types need flash attention for V and a head dimension that is a multiple of 32.
    pub fn is_quantized(self) -> bool {
        !matches!(self, CacheType::F32 | CacheType::F16 | CacheType::Bf16)
    }
}

/// The three KV types the planner offers. Ollama applies one type to both K and V
/// (`OLLAMA_KV_CACHE_TYPE`), so this is one choice, not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvType {
    F16,
    Q8_0,
    Q4_0,
}

impl KvType {
    pub const ALL: [KvType; 3] = [KvType::F16, KvType::Q8_0, KvType::Q4_0];

    pub fn index(self) -> usize {
        match self {
            KvType::F16 => 0,
            KvType::Q8_0 => 1,
            KvType::Q4_0 => 2,
        }
    }

    pub fn cache(self) -> CacheType {
        match self {
            KvType::F16 => CacheType::F16,
            KvType::Q8_0 => CacheType::Q8_0,
            KvType::Q4_0 => CacheType::Q4_0,
        }
    }

    pub fn bytes_per_32(self) -> u64 {
        self.cache().bytes_per_32()
    }

    /// The value `OLLAMA_KV_CACHE_TYPE` takes.
    pub fn ollama_name(self) -> &'static str {
        self.cache().name()
    }

    /// Short label for narrow places: `f16`, `q8`, `q4`.
    pub fn short(self) -> &'static str {
        match self {
            KvType::F16 => "f16",
            KvType::Q8_0 => "q8",
            KvType::Q4_0 => "q4",
        }
    }

    pub fn parse(s: &str) -> Option<KvType> {
        match CacheType::parse(s)? {
            CacheType::F16 => Some(KvType::F16),
            CacheType::Q8_0 => Some(KvType::Q8_0),
            CacheType::Q4_0 => Some(KvType::Q4_0),
            _ => None,
        }
    }

    pub fn from_cache(c: CacheType) -> Option<KvType> {
        KvType::parse(c.name())
    }
}

impl std::fmt::Display for KvType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.ollama_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_sizes_match_ggml() {
        let q4k = GgmlType::from_name("q4_k").unwrap();
        // One 5120x248320 Q4_K matrix: 20 blocks per row of 144 bytes.
        assert_eq!(q4k.tensor_bytes(&[5120, 248_320]), 20 * 144 * 248_320);
        assert_eq!(GgmlType::from_id(18).unwrap().name, "IQ3_XXS");
        assert_eq!(GgmlType::from_name("MXFP4").unwrap().tensor_bytes(&[64]), 34);
        assert_eq!(GgmlType::from_name("F32").unwrap().tensor_bytes(&[5120]), 20_480);
        assert!(GgmlType::from_name("Q9_Z").is_none());
    }

    #[test]
    fn kv_bytes_per_element() {
        assert_eq!(KvType::F16.cache().bytes_for(256), 512);
        assert_eq!(KvType::Q8_0.cache().bytes_for(256), 272);
        assert_eq!(KvType::Q4_0.cache().bytes_for(256), 144);
        assert_eq!(KvType::parse("Q8_0"), Some(KvType::Q8_0));
        assert_eq!(KvType::parse("q5_1"), None);
        assert_eq!(CacheType::parse("iq4_nl"), Some(CacheType::Iq4Nl));
    }
}
