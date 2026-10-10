//! A model's metadata and tensor list, from a GGUF header or from Ollama's `/api/show`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::ggml::GgmlType;

/// One tensor: name, ggml type (`None` when the type is newer than this table) and shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub name: String,
    pub ty: Option<GgmlType>,
    /// The type as the source named it, kept for messages when `ty` is unknown.
    pub ty_name: String,
    pub shape: Vec<u64>,
}

impl Tensor {
    /// Bytes on disk and in memory; 0 when the type is unknown.
    pub fn bytes(&self) -> u64 {
        self.ty.map_or(0, |t| t.tensor_bytes(&self.shape))
    }

    /// The block index of a `blk.N.*` tensor.
    pub fn layer(&self) -> Option<u32> {
        let rest = self.name.strip_prefix("blk.")?;
        rest.split('.').next()?.parse().ok()
    }
}

/// Key-value metadata plus the tensor list. Arrays too large to matter here (the tokenizer's) are
/// left out by the readers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelHeader {
    pub kv: BTreeMap<String, Value>,
    pub tensors: Vec<Tensor>,
}

impl ModelHeader {
    /// From `/api/show` (`model_info` and `tensors`), or a fixture in the same shape.
    pub fn from_show(v: &Value) -> Result<ModelHeader, String> {
        let info = v.get("model_info").and_then(Value::as_object).ok_or("no `model_info` object")?;
        let kv = info.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut tensors = Vec::new();
        for t in v.get("tensors").and_then(Value::as_array).into_iter().flatten() {
            let name = t.get("name").and_then(Value::as_str).ok_or("a tensor without a name")?;
            let ty_name = t.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let shape = t
                .get("shape")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            tensors.push(Tensor { name: name.to_string(), ty: GgmlType::from_name(&ty_name), ty_name, shape });
        }
        Ok(ModelHeader { kv, tensors })
    }

    pub fn arch(&self) -> Option<&str> {
        self.kv.get("general.architecture").and_then(Value::as_str)
    }

    /// `{arch}.{key}`.
    pub fn a(&self, key: &str) -> Option<&Value> {
        let arch = self.arch()?;
        self.kv.get(&format!("{arch}.{key}")).filter(|v| !v.is_null())
    }

    pub fn a_u64(&self, key: &str) -> Option<u64> {
        let v = self.a(key)?;
        v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
    }

    /// A per-layer value: a scalar applies to every layer, an array gives one per layer.
    pub fn a_layer(&self, key: &str, layer: u32) -> Option<u64> {
        match self.a(key)? {
            Value::Array(a) => a.get(layer as usize).and_then(Value::as_u64),
            v => v.as_u64(),
        }
    }

    pub fn a_is_array(&self, key: &str) -> bool {
        matches!(self.a(key), Some(Value::Array(_)))
    }

    /// Tensor names of block `layer`, as suffixes (`attn_k.weight`).
    pub fn layer_tensors(&self, layer: u32) -> impl Iterator<Item = &str> {
        let prefix = format!("blk.{layer}.");
        self.tensors.iter().filter_map(move |t| t.name.strip_prefix(prefix.as_str()))
    }

    pub fn tensor(&self, name: &str) -> Option<&Tensor> {
        self.tensors.iter().find(|t| t.name == name)
    }

    pub fn general_u64(&self, key: &str) -> Option<u64> {
        self.kv.get(&format!("general.{key}")).and_then(Value::as_u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn show_shape_reads_keys_and_tensors() {
        let h = ModelHeader::from_show(&json!({
            "model_info": {"general.architecture": "llama", "llama.block_count": 2,
                           "llama.attention.head_count_kv": [8, 0]},
            "tensors": [{"name": "blk.1.attn_k.weight", "type": "Q4_0", "shape": [4096, 1024]},
                        {"name": "output.weight", "type": "NEW_TYPE", "shape": [4096, 10]}]
        }))
        .unwrap();
        assert_eq!(h.a_u64("block_count"), Some(2));
        assert_eq!(h.a_layer("attention.head_count_kv", 1), Some(0));
        assert_eq!(h.tensors[0].layer(), Some(1));
        assert_eq!(h.tensors[0].bytes(), 4096 / 32 * 18 * 1024);
        assert_eq!(h.tensors[1].bytes(), 0);
        assert_eq!(h.layer_tensors(1).collect::<Vec<_>>(), vec!["attn_k.weight"]);
    }
}
