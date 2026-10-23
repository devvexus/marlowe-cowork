//! Project values on the wire: the patch a client applies to its copy of a project's state JSON.
//! The state itself, its schema and the store live in `marlowe-project`; this file holds only what
//! crosses the socket, so clients can follow a project without depending on that crate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a [`ProjectPatch`] does at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOp {
    /// Put `value` at `path` (an object key is created, an existing value replaced).
    Set,
    /// Delete the object key at `path`.
    Remove,
}

/// One change to a project's state JSON: an RFC 6901 JSON pointer into the state and the new
/// value there. Arrays are replaced whole (a `Set` on the array's own path), never edited by index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectPatch {
    /// Set or remove.
    pub op: PatchOp,
    /// JSON pointer into the state document, e.g. `/items/i_1a2b3c4d/status`.
    pub path: String,
    /// The new value; absent for [`PatchOp::Remove`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

impl ProjectPatch {
    /// A set operation.
    pub fn set(path: impl Into<String>, value: Value) -> Self {
        ProjectPatch { op: PatchOp::Set, path: path.into(), value: Some(value) }
    }

    /// A remove operation.
    pub fn remove(path: impl Into<String>) -> Self {
        ProjectPatch { op: PatchOp::Remove, path: path.into(), value: None }
    }
}

/// One answer a gate offers (a button on a plan or checkin card).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGateOption {
    /// Stable id the answer is sent back with (`approve`, `keep_researching`, `steer`, `cancel`).
    pub id: String,
    /// Words on the button.
    pub label: String,
    /// The answer needs the user's text (steer).
    #[serde(default)]
    pub needs_text: bool,
}
