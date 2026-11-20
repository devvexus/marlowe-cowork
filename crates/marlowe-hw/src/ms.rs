//! MS ("Mad Shift") files: GGUFs written by ms-llama.cpp's quantizer, which only that backend loads.
//!
//! The mark is the metadata: every MS file carries `msffn.*` keys (the main tiers `msffn.hadamard`,
//! `msffn.rot_weight_names`, `msffn.rot_seeds`, `msffn.quant_name`; the MTP sidecar
//! `msffn.quant_name = MS-MTP`). `prism.hadamard.*` alone is not MS: stock llama.cpp supports it
//! for PrismML/Bonsai models. The tensor types only support the call, because upstream now uses
//! type 43 (and file type 42) for `F8_E4M3`, so neither proves MS alone.
//!
//! Two generations: Gen 1 (ms-v1.0, every file published today) uses ggml types 43..48; Gen 2 (the
//! next MS release) moves them to 201..206 with 200..215 reserved. A newer ms-llama.cpp rejects a
//! Gen 1 file, so the engine checks the installed version against the file's generation.

use crate::header::ModelHeader;

/// The metadata prefix every MS file carries.
pub const KEY_PREFIX: &str = "msffn.";

/// Which MS tensor-type numbering a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MsGen {
    /// ms-v1.0: types 43..48.
    Gen1,
    /// The next release: types 200..215 reserved (201..206 in use).
    Gen2,
}

impl MsGen {
    pub fn number(self) -> u8 {
        match self {
            MsGen::Gen1 => 1,
            MsGen::Gen2 => 2,
        }
    }

    pub fn from_number(n: u8) -> Option<MsGen> {
        match n {
            1 => Some(MsGen::Gen1),
            2 => Some(MsGen::Gen2),
            _ => None,
        }
    }
}

impl std::fmt::Display for MsGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MS Gen {}", self.number())
    }
}

/// What a header says about its MS-ness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsInfo {
    /// `None` when the file carries the keys but no MS tensor types to tell the generation by.
    pub generation: Option<MsGen>,
    /// `msffn.quant_name` (`MS-MTP` for the sidecar).
    pub quant: Option<String>,
    /// The multi-token-prediction sidecar, which runs only beside its main model.
    pub mtp: bool,
    /// The MS tensor type ids the file holds.
    pub types: Vec<u32>,
}

impl MsInfo {
    /// `MS Gen 1`, or `MS` when the generation is not told.
    pub fn label(&self) -> String {
        self.generation.map_or_else(|| "MS".to_string(), |g| g.to_string())
    }
}

/// Whether tensor type `id` is one only an MS build writes (44..48, 200..215): upstream may reuse
/// 43 (`F8_E4M3`), and any other id it does not know yet is not proof of MS.
pub fn only_ms_type(id: u32) -> bool {
    (44..=48).contains(&id) || (200..=215).contains(&id)
}

/// Whether tensor type `id` is one of the MS numberings (Gen 1 43..48, Gen 2 200..215).
fn is_ms_type(id: u32) -> bool {
    (43..=48).contains(&id) || (200..=215).contains(&id)
}

impl ModelHeader {
    /// `Some` for an MS file: by the `msffn.` metadata, or by types only an MS build has (44..48,
    /// 200..215). Never by the file's name.
    pub fn ms(&self) -> Option<MsInfo> {
        let keyed = self.kv.keys().chain(self.skipped.iter()).any(|k| k.starts_with(KEY_PREFIX));
        let ids = |keep: fn(u32) -> bool| -> Vec<u32> {
            let mut ids: Vec<u32> = self
                .tensors
                .iter()
                .filter(|t| t.ty.is_none())
                .filter_map(|t| t.ty_name.strip_prefix("type ")?.parse().ok())
                .filter(|id| keep(*id))
                .collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let types = ids(is_ms_type);
        let only_ms = ids(only_ms_type);
        if !keyed && only_ms.is_empty() {
            return None;
        }
        let generation = if types.iter().any(|t| (200..=215).contains(t)) {
            Some(MsGen::Gen2)
        } else if types.iter().any(|t| (43..=48).contains(t)) {
            Some(MsGen::Gen1)
        } else {
            None
        };
        let quant = self.kv.get("msffn.quant_name").and_then(|v| v.as_str()).map(str::to_string);
        let mtp = quant.as_deref().is_some_and(|q| q.eq_ignore_ascii_case("MS-MTP")) || self.kv.contains_key("msffn.code");
        Some(MsInfo { generation, quant, mtp, types })
    }
}

#[cfg(test)]
mod tests {
    use crate::gguf::{parse_header, write_header};
    use serde_json::json;

    use super::*;

    fn header(kv: &[(&str, serde_json::Value)], types: &[u32]) -> ModelHeader {
        let tensors: Vec<(String, u32, Vec<u64>)> = types.iter().enumerate().map(|(i, t)| (format!("blk.{i}.w"), *t, vec![256])).collect();
        let refs: Vec<(&str, u32, Vec<u64>)> = tensors.iter().map(|(n, t, s)| (n.as_str(), *t, s.clone())).collect();
        parse_header(&write_header(kv, &refs)[..]).unwrap()
    }

    #[test]
    fn the_msffn_keys_make_a_file_ms_and_the_types_tell_the_generation() {
        let kv = [("general.architecture", json!("qwen35")), ("msffn.quant_name", json!("MS2.7")), ("msffn.hadamard", json!(true))];
        let gen1 = header(&kv, &[0, 45, 47]).ms().unwrap();
        assert_eq!((gen1.generation, gen1.types.clone(), gen1.mtp), (Some(MsGen::Gen1), vec![45, 47], false));
        assert_eq!(gen1.label(), "MS Gen 1");
        assert_eq!(header(&kv, &[0, 202]).ms().unwrap().generation, Some(MsGen::Gen2));
        // The keys with no MS type: MS, generation not told.
        let bare = header(&kv, &[0, 12]).ms().unwrap();
        assert_eq!((bare.generation, bare.label()), (None, "MS".to_string()));
    }

    #[test]
    fn the_sidecar_is_an_mtp_file() {
        let kv = [("general.architecture", json!("qwen35")), ("msffn.quant_name", json!("MS-MTP")), ("msffn.code", json!(1))];
        assert!(header(&kv, &[47]).ms().unwrap().mtp);
    }

    #[test]
    fn prism_hadamard_and_the_shared_type_43_alone_are_not_ms() {
        let prism = [("general.architecture", json!("qwen3")), ("prism.hadamard.enabled", json!(true))];
        assert_eq!(header(&prism, &[0, 8]).ms(), None);
        // Upstream's own F8_E4M3 is type 43.
        assert_eq!(header(&[("general.architecture", json!("llama"))], &[0, 43]).ms(), None);
        // A type only an MS build has is MS even where the keys were stripped.
        assert!(header(&[("general.architecture", json!("llama"))], &[46]).ms().is_some());
    }

    #[test]
    fn a_long_msffn_array_that_was_skipped_still_marks_the_file() {
        let seeds: Vec<serde_json::Value> = (0..5000).map(|i| json!(i)).collect();
        let h = header(&[("general.architecture", json!("qwen35")), ("msffn.rot_seeds", serde_json::Value::Array(seeds))], &[0]);
        assert!(!h.kv.contains_key("msffn.rot_seeds"));
        assert!(h.ms().is_some());
    }
}
