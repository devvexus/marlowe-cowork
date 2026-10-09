//! Belief vectors persisted beside the snapshot, so boot does not re-embed the store.
//!
//! Sealed like the snapshot (blake3 keyed MAC, key derived from the profile key) and bound to
//! the embedder's pinned digests. Each record carries a digest of the text it was computed
//! from; a record whose belief is gone, tombstoned or re-worded is dropped on load. The file is
//! rewritten after every forget, so a vector does not outlive its text on disk.

use std::fs;
use std::path::Path;

use marlowe_journal::SigningKey;
use marlowe_memory::cue::dense::embedder::{MODEL_SHA256, VOCAB_SHA256};
use marlowe_memory::cue::dense::vectors::VectorStore;
use marlowe_memory::cue::dense::DIMENSIONS;
use marlowe_memory::BeliefStore;

use crate::error::MindError;

/// Vector file inside the profile dir.
pub(crate) const VECTORS_FILE: &str = "beliefs.vectors";

const MAGIC: &[u8] = b"marlowe-belief-vectors\n";
const FORMAT: u32 = 1;
const KEY_CONTEXT: &str = "marlowe belief vectors v1";

fn seal_key(key: &SigningKey) -> [u8; 32] {
    let hex = key.to_hex();
    let mut bytes = [0u8; 32];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).unwrap_or("00");
        bytes[i] = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    blake3::derive_key(KEY_CONTEXT, &bytes)
}

fn header() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(MODEL_SHA256.as_bytes());
    out.extend_from_slice(VOCAB_SHA256.as_bytes());
    out.extend_from_slice(&(DIMENSIONS as u32).to_le_bytes());
    out
}

/// Writes every live belief's vector atomically (temp file + rename). Returns how many.
pub(crate) fn write(profile_dir: &Path, key: &SigningKey, beliefs: &BeliefStore, vectors: &VectorStore) -> Result<usize, MindError> {
    let mut body = header();
    let records: Vec<(&str, &str, &[f32])> = beliefs
        .entries()
        .filter(|e| e.fidelity > marlowe_contract::Fidelity::Tombstone)
        .filter_map(|e| vectors.get(&e.id).map(|v| (e.id.as_str(), e.text.as_str(), v)))
        .filter(|(_, _, v)| v.len() == DIMENSIONS)
        .collect();
    body.extend_from_slice(&(records.len() as u64).to_le_bytes());
    body.reserve(records.len() * (DIMENSIONS * 4 + 64));
    for (id, text, v) in &records {
        body.extend_from_slice(&(id.len() as u32).to_le_bytes());
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(blake3::hash(text.as_bytes()).as_bytes());
        for x in v.iter() {
            body.extend_from_slice(&x.to_le_bytes());
        }
    }
    let mac = blake3::keyed_hash(&seal_key(key), &body);
    let mut out = Vec::with_capacity(MAGIC.len() + 32 + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(mac.as_bytes());
    out.extend_from_slice(&body);
    let path = profile_dir.join(VECTORS_FILE);
    let tmp = profile_dir.join(format!("{VECTORS_FILE}.tmp"));
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &path)?;
    Ok(records.len())
}

/// Installs the stored vectors that still match their belief's text. `Ok(0)` when there is no
/// file; `Err` (with the reason) when the file is unusable, which the caller logs and rebuilds.
pub(crate) fn load(profile_dir: &Path, key: &SigningKey, beliefs: &BeliefStore, vectors: &mut VectorStore) -> Result<usize, String> {
    let bytes = match fs::read(profile_dir.join(VECTORS_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.to_string()),
    };
    let rest = bytes.strip_prefix(MAGIC).ok_or("not a vector file")?;
    if rest.len() < 32 {
        return Err("truncated before the mac".into());
    }
    let (mac, body) = rest.split_at(32);
    let claimed = blake3::Hash::from_bytes(mac.try_into().map_err(|_| "bad mac")?);
    // blake3::Hash equality is constant-time.
    if blake3::keyed_hash(&seal_key(key), body) != claimed {
        return Err("mac does not verify under this profile's key".into());
    }
    let head = header();
    let body = body.strip_prefix(head.as_slice()).ok_or("written by another embedder or format")?;
    let mut r = Reader(body);
    let count = r.u64()?;
    let mut installed = 0;
    for _ in 0..count {
        let id_len = r.u32()? as usize;
        let id = std::str::from_utf8(r.take(id_len)?).map_err(|_| "id is not utf-8")?.to_string();
        let digest = r.take(32)?;
        let mut v = Vec::with_capacity(DIMENSIONS);
        for _ in 0..DIMENSIONS {
            v.push(f32::from_le_bytes(r.take(4)?.try_into().map_err(|_| "short float")?));
        }
        if let Some(e) = beliefs.get(&id) {
            if blake3::hash(e.text.as_bytes()).as_bytes() == digest {
                installed += usize::from(vectors.install(beliefs, &id, &e.text, v));
            }
        }
    }
    if !r.0.is_empty() {
        return Err("trailing bytes".into());
    }
    Ok(installed)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.0.len() < n {
            return Err("truncated".into());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| "short u32")?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "short u64")?))
    }
}
