//! A conversation's prompt cache across a stop of the server.
//!
//! llama-server keeps each slot's KV cache in memory only, so a stop (a restart of Marlowe, an
//! idle unload, a model switch) would make every conversation read its whole history again. Before
//! the engine stops a server it asks it to write each used slot to disk (`--slot-save-path`,
//! `POST /slots/{id}?action=save`), and after the next load of the same model in the same shape it
//! reads them back (`action=restore`) and gives each slot its conversation again. The server
//! checks the restored tokens against the next prompt itself, so a stale file only costs a
//! re-read, never a wrong answer. Everything here is best effort: a failure is a cold cache.

use std::path::{Path, PathBuf};
use std::time::Duration;

use marlowe_model::EngineShape;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::eligible::Chosen;
use crate::http::{self, LocalEndpoint};

const SLOT_IO: Duration = Duration::from_secs(120);

#[derive(Serialize, Deserialize)]
struct Entry {
    slot: u32,
    key: String,
    file: String,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    model: String,
    blob: String,
    slots: u16,
    ctx_per_slot: u32,
    kv: String,
    /// The backend and version that computed the state (`""`: a developer's own binary, or a
    /// manifest from before this was recorded).
    #[serde(default)]
    backend: String,
    entries: Vec<Entry>,
}

/// `llama.cpp v1.0.0`: what a saved cache is tied to. A state computed by another build is never
/// loaded into this one (another backend's kernels, or a changed state format).
fn build_of(chosen: Option<&Chosen>) -> String {
    chosen.map(|c| format!("{} {}", c.backend.id(), c.tag.as_deref().unwrap_or(""))).unwrap_or_default()
}

fn stem(model: &str) -> String {
    model.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

fn manifest_path(dir: &Path, model: &str) -> PathBuf {
    dir.join(format!("{}.slots.json", stem(model)))
}

/// Writes every used slot (`slot index`, `conversation key`) of the running server to `dir` and
/// the manifest that says whose they are. Returns how many were saved.
pub fn save(endpoint: &LocalEndpoint, dir: &Path, model: &str, blob: &Path, shape: &EngineShape, chosen: Option<&Chosen>, used: &[(u32, u64)]) -> usize {
    let _ = std::fs::remove_file(manifest_path(dir, model));
    let mut entries = Vec::new();
    for &(slot, key) in used {
        let file = format!("{}-{slot}.bin", stem(model));
        let ok = http::post_json(endpoint, &format!("/slots/{slot}?action=save"), &json!({"filename": file}), SLOT_IO).is_ok();
        if ok {
            entries.push(Entry { slot, key: key.to_string(), file });
        }
    }
    let saved = entries.len();
    let manifest = Manifest {
        model: model.to_string(),
        blob: blob.display().to_string(),
        slots: shape.slots,
        ctx_per_slot: shape.ctx_per_slot,
        kv: shape.kv.clone(),
        backend: build_of(chosen),
        entries,
    };
    if saved > 0 {
        if let Ok(text) = serde_json::to_string(&manifest) {
            let _ = std::fs::write(manifest_path(dir, model), text);
        }
    }
    saved
}

/// Reads the saved slots back into a freshly started server, when it is the same model in the
/// same shape. Returns the (slot, conversation key) pairs that were restored.
pub fn restore(endpoint: &LocalEndpoint, dir: &Path, model: &str, blob: &Path, shape: &EngineShape, chosen: Option<&Chosen>) -> Vec<(u32, u64)> {
    let path = manifest_path(dir, model);
    let Some(m) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Manifest>(&t).ok()) else {
        return Vec::new();
    };
    // Consumed: the next stop writes a new one, and a crash must not restore an older state twice.
    let _ = std::fs::remove_file(&path);
    if m.blob != blob.display().to_string() || m.slots != shape.slots || m.ctx_per_slot != shape.ctx_per_slot || m.kv != shape.kv || m.backend != build_of(chosen) {
        return Vec::new();
    }
    let mut back = Vec::new();
    for e in m.entries {
        let Ok(key) = e.key.parse::<u64>() else { continue };
        if e.slot >= u32::from(shape.slots) || !dir.join(&e.file).is_file() {
            continue;
        }
        if http::post_json(endpoint, &format!("/slots/{}?action=restore", e.slot), &json!({"filename": e.file}), SLOT_IO).is_ok() {
            back.push((e.slot, key));
        }
    }
    back
}

/// Writes the one slot of a one-slot model to its own file, so another conversation can use the
/// slot and this one's cache comes back when it returns. `None`: the server did not save it.
pub fn park(endpoint: &LocalEndpoint, dir: &Path, model: &str, slot: u32, key: u64) -> Option<String> {
    let file = format!("{}-k{key}.bin", stem(model));
    http::post_json(endpoint, &format!("/slots/{slot}?action=save"), &json!({"filename": file}), SLOT_IO).ok()?;
    dir.join(&file).is_file().then_some(file)
}

/// Reads a parked cache back into `slot`.
pub fn unpark(endpoint: &LocalEndpoint, dir: &Path, slot: u32, file: &str) -> bool {
    dir.join(file).is_file() && http::post_json(endpoint, &format!("/slots/{slot}?action=restore"), &json!({"filename": file}), SLOT_IO).is_ok()
}

/// Deletes a parked cache that will not be asked for again.
pub fn discard(dir: &Path, file: &str) {
    let _ = std::fs::remove_file(dir.join(file));
}
