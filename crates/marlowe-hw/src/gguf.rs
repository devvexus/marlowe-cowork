//! A header-only GGUF reader: the key-value pairs and tensor infos, never the tensor data.
//!
//! Arrays longer than [`MAX_ARRAY`] (the tokenizer's vocabulary and merges) are read past and
//! left out, so a 10 GiB blob costs a few megabytes of reading. Every count and length is
//! bounded, so a damaged file is an error rather than an allocation.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use serde_json::{json, Value};

use crate::ggml::GgmlType;
use crate::header::{ModelHeader, Tensor};

/// Arrays longer than this are skipped. Per-layer arrays are at most a few hundred long.
pub const MAX_ARRAY: u64 = 4096;
const MAX_STRING: u64 = 1 << 26;
const MAX_COUNT: u64 = 1 << 24;
const MAX_DIMS: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum GgufError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("not a GGUF file (magic {0:?})")]
    Magic([u8; 4]),
    #[error("GGUF version {0} is not supported (2 and 3 are)")]
    Version(u32),
    #[error("malformed GGUF header: {0}")]
    Malformed(String),
}

/// Reads the header of the GGUF at `path`.
pub fn read_header(path: &Path) -> Result<ModelHeader, GgufError> {
    parse_header(BufReader::with_capacity(1 << 20, File::open(path)?))
}

/// Reads a GGUF header from any reader positioned at the start of the file.
pub fn parse_header<R: Read>(mut r: R) -> Result<ModelHeader, GgufError> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Err(GgufError::Magic(magic));
    }
    let version = u32_(&mut r)?;
    if !(2..=3).contains(&version) {
        return Err(GgufError::Version(version));
    }
    let n_tensors = bounded(u64_(&mut r)?, MAX_COUNT, "tensor count")?;
    let n_kv = bounded(u64_(&mut r)?, MAX_COUNT, "key count")?;
    let mut kv = BTreeMap::new();
    for _ in 0..n_kv {
        let key = string(&mut r)?;
        let ty = u32_(&mut r)?;
        if let Some(v) = value(&mut r, ty)? {
            kv.insert(key, v);
        }
    }
    let mut tensors = Vec::with_capacity(n_tensors.min(65_536) as usize);
    for _ in 0..n_tensors {
        let name = string(&mut r)?;
        let dims = u32_(&mut r)?;
        if dims > MAX_DIMS {
            return Err(GgufError::Malformed(format!("tensor {name} has {dims} dimensions")));
        }
        let shape = (0..dims).map(|_| u64_(&mut r)).collect::<Result<Vec<_>, _>>()?;
        let id = u32_(&mut r)?;
        let _offset = u64_(&mut r)?;
        let ty = GgmlType::from_id(id);
        let ty_name = ty.map_or_else(|| format!("type {id}"), |t| t.name.to_string());
        tensors.push(Tensor { name, ty, ty_name, shape });
    }
    Ok(ModelHeader { kv, tensors })
}

fn bounded(n: u64, max: u64, what: &str) -> Result<u64, GgufError> {
    if n > max {
        return Err(GgufError::Malformed(format!("{what} {n} is implausible")));
    }
    Ok(n)
}

fn u32_<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn u64_<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn bytes<R: Read, const N: usize>(r: &mut R) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn string<R: Read>(r: &mut R) -> Result<String, GgufError> {
    let n = bounded(u64_(r)?, MAX_STRING, "string length")?;
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn skip<R: Read>(r: &mut R, n: u64) -> io::Result<()> {
    let got = io::copy(&mut r.take(n), &mut io::sink())?;
    if got != n {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated GGUF header"));
    }
    Ok(())
}

/// Bytes of one scalar of type `ty`, `None` for strings and arrays.
fn scalar_size(ty: u32) -> Option<u64> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// One value. `None` for an array that was skipped.
fn value<R: Read>(r: &mut R, ty: u32) -> Result<Option<Value>, GgufError> {
    Ok(Some(match ty {
        0 => json!(bytes::<_, 1>(r)?[0]),
        1 => json!(bytes::<_, 1>(r)?[0] as i8),
        2 => json!(u16::from_le_bytes(bytes(r)?)),
        3 => json!(i16::from_le_bytes(bytes(r)?)),
        4 => json!(u32::from_le_bytes(bytes(r)?)),
        5 => json!(i32::from_le_bytes(bytes(r)?)),
        6 => json!(f32::from_le_bytes(bytes(r)?)),
        7 => json!(bytes::<_, 1>(r)?[0] != 0),
        8 => Value::String(string(r)?),
        10 => json!(u64::from_le_bytes(bytes(r)?)),
        11 => json!(i64::from_le_bytes(bytes(r)?)),
        12 => json!(f64::from_le_bytes(bytes(r)?)),
        9 => {
            let et = u32_(r)?;
            let n = bounded(u64_(r)?, 1 << 32, "array length")?;
            if n > MAX_ARRAY {
                match scalar_size(et) {
                    Some(s) => skip(r, s * n)?,
                    None => {
                        for _ in 0..n {
                            value(r, et)?;
                        }
                    }
                }
                return Ok(None);
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                out.push(value(r, et)?.unwrap_or(Value::Null));
            }
            Value::Array(out)
        }
        other => return Err(GgufError::Malformed(format!("value type {other}"))),
    }))
}

/// Writes a minimal GGUF header (tests, fixtures): `kv` as (key, value) and tensors as
/// (name, type id, shape). Only the value kinds the planner reads are supported.
pub fn write_header(kv: &[(&str, Value)], tensors: &[(&str, u32, Vec<u64>)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
    b.extend_from_slice(&(kv.len() as u64).to_le_bytes());
    let s = |b: &mut Vec<u8>, s: &str| {
        b.extend_from_slice(&(s.len() as u64).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    };
    fn put(b: &mut Vec<u8>, v: &Value, with_type: bool) {
        let ty: u32 = match v {
            Value::Bool(_) => 7,
            Value::String(_) => 8,
            Value::Array(_) => 9,
            Value::Number(n) if n.is_f64() => 6,
            _ => 4,
        };
        if with_type {
            b.extend_from_slice(&ty.to_le_bytes());
        }
        match v {
            Value::Bool(x) => b.push(u8::from(*x)),
            Value::String(x) => {
                b.extend_from_slice(&(x.len() as u64).to_le_bytes());
                b.extend_from_slice(x.as_bytes());
            }
            Value::Array(a) => {
                let et: u32 = match a.first() {
                    Some(Value::Bool(_)) => 7,
                    Some(Value::String(_)) => 8,
                    Some(Value::Number(n)) if n.is_f64() => 6,
                    _ => 4,
                };
                b.extend_from_slice(&et.to_le_bytes());
                b.extend_from_slice(&(a.len() as u64).to_le_bytes());
                for x in a {
                    put(b, x, false);
                }
            }
            Value::Number(n) if n.is_f64() => b.extend_from_slice(&(n.as_f64().unwrap_or(0.0) as f32).to_le_bytes()),
            Value::Number(n) => b.extend_from_slice(&(n.as_u64().unwrap_or(0) as u32).to_le_bytes()),
            Value::Null | Value::Object(_) => {}
        }
    }
    for (k, v) in kv {
        s(&mut b, k);
        put(&mut b, v, true);
    }
    for (name, ty, shape) in tensors {
        s(&mut b, name);
        b.extend_from_slice(&(shape.len() as u32).to_le_bytes());
        for d in shape {
            b.extend_from_slice(&d.to_le_bytes());
        }
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_keys_arrays_and_tensors_and_skips_long_arrays() {
        let vocab: Vec<Value> = (0..5000).map(|i| json!(format!("tok{i}"))).collect();
        let bytes = write_header(
            &[
                ("general.architecture", json!("qwen35")),
                ("qwen35.block_count", json!(4)),
                ("qwen35.attention.recurrent_layers", json!([true, true, true, false])),
                ("qwen35.rope.freq_base", json!(10000.0)),
                ("tokenizer.ggml.tokens", Value::Array(vocab)),
            ],
            &[("blk.3.attn_k.weight", 8, vec![5120, 1024]), ("token_embd.weight", 12, vec![5120, 1000])],
        );
        let h = parse_header(&bytes[..]).unwrap();
        assert_eq!(h.arch(), Some("qwen35"));
        assert_eq!(h.a_u64("block_count"), Some(4));
        assert_eq!(h.a("attention.recurrent_layers"), Some(&json!([true, true, true, false])));
        assert!(!h.kv.contains_key("tokenizer.ggml.tokens"));
        assert_eq!(h.tensors.len(), 2);
        assert_eq!(h.tensors[0].ty.unwrap().name, "Q8_0");
        assert_eq!(h.tensors[0].bytes(), 5120 / 32 * 34 * 1024);
    }

    #[test]
    fn rejects_what_is_not_a_gguf_and_truncation() {
        assert!(matches!(parse_header(&b"GGML\x03\0\0\0"[..]), Err(GgufError::Magic(_))));
        let mut bytes = write_header(&[("general.architecture", json!("llama"))], &[("a", 0, vec![4])]);
        bytes.truncate(bytes.len() - 4);
        assert!(parse_header(&bytes[..]).is_err());
        let mut v1 = b"GGUF".to_vec();
        v1.extend_from_slice(&1u32.to_le_bytes());
        assert!(matches!(parse_header(&v1[..]), Err(GgufError::Version(1))));
    }
}
