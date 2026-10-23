//! The client side of live updates: a copy of a project's state JSON that follows the patch
//! stream, and the pointer arithmetic to apply one.
//!
//! A client that attaches late asks for the full state ([`Replica::from_state`]); after that every
//! `ProjectUpdated { base, rev, patches }` is applied with [`Replica::apply`]. If `base` is not
//! the revision the copy is at, a patch was missed: [`Replica::apply`] answers
//! [`ReplicaError::Gap`] and the client asks for the full state again.

use marlowe_proto::{PatchOp, ProjectPatch};
use serde_json::Value;
use thiserror::Error;

/// Why a patch could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReplicaError {
    /// The update starts at a revision the copy is not at: ask for the full state.
    #[error("update starts at revision {base} but the copy is at {have}")]
    Gap {
        /// The revision the copy is at.
        have: u64,
        /// The revision the update starts from.
        base: u64,
    },
    /// A patch path does not lead into the document.
    #[error("patch path `{0}` does not lead into the state")]
    BadPath(String),
}

fn unescape(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

fn split(path: &str) -> Result<Vec<String>, ReplicaError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let rest = path.strip_prefix('/').ok_or_else(|| ReplicaError::BadPath(path.to_string()))?;
    Ok(rest.split('/').map(unescape).collect())
}

/// Applies patches in order to a JSON document. A set creates the key it names inside an existing
/// object; a remove of a missing key is not an error. Anything that would have to step into an
/// array or a scalar is a [`ReplicaError::BadPath`].
pub fn apply_patches(doc: &mut Value, patches: &[ProjectPatch]) -> Result<(), ReplicaError> {
    for p in patches {
        let segs = split(&p.path)?;
        let Some((last, parents)) = segs.split_last() else {
            // The root itself.
            match (&p.op, &p.value) {
                (PatchOp::Set, Some(v)) => *doc = v.clone(),
                _ => return Err(ReplicaError::BadPath(p.path.clone())),
            }
            continue;
        };
        let mut cur = &mut *doc;
        for s in parents {
            cur = cur.as_object_mut().and_then(|o| o.get_mut(s)).ok_or_else(|| ReplicaError::BadPath(p.path.clone()))?;
        }
        let obj = cur.as_object_mut().ok_or_else(|| ReplicaError::BadPath(p.path.clone()))?;
        match (&p.op, &p.value) {
            (PatchOp::Set, Some(v)) => {
                obj.insert(last.clone(), v.clone());
            }
            (PatchOp::Remove, _) => {
                obj.remove(last);
            }
            (PatchOp::Set, None) => return Err(ReplicaError::BadPath(p.path.clone())),
        }
    }
    Ok(())
}

/// A client's copy of one project's state and the revision it is at.
#[derive(Debug, Clone, PartialEq)]
pub struct Replica {
    /// The state document.
    pub state: Value,
    /// Its revision.
    pub rev: u64,
}

impl Replica {
    /// A copy taken from a full state (`rev` is read from the document).
    pub fn from_state(state: Value) -> Replica {
        let rev = state.get("rev").and_then(Value::as_u64).unwrap_or(0);
        Replica { state, rev }
    }

    /// Applies an update that takes the copy from `base` to `rev`. On a gap or a bad path the
    /// copy is left as it was.
    pub fn apply(&mut self, base: u64, rev: u64, patches: &[ProjectPatch]) -> Result<(), ReplicaError> {
        if base != self.rev {
            return Err(ReplicaError::Gap { have: self.rev, base });
        }
        let mut next = self.state.clone();
        apply_patches(&mut next, patches)?;
        self.state = next;
        self.rev = rev;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn set_creates_keys_and_remove_deletes_them() {
        let mut d = json!({"a": {"b": 1}, "rev": 1});
        apply_patches(&mut d, &[ProjectPatch::set("/a/c", json!(2)), ProjectPatch::remove("/a/b"), ProjectPatch::remove("/a/zzz"), ProjectPatch::set("/rev", json!(2))]).unwrap();
        assert_eq!(d, json!({"a": {"c": 2}, "rev": 2}));
    }

    #[test]
    fn escaped_segments_round_trip() {
        let mut d = json!({"m": {}});
        apply_patches(&mut d, &[ProjectPatch::set("/m/a~1b~0c", json!(1))]).unwrap();
        assert_eq!(d, json!({"m": {"a/b~c": 1}}));
    }

    #[test]
    fn paths_that_do_not_lead_in_are_refused() {
        let mut d = json!({"a": [1, 2], "s": "x"});
        for bad in ["/missing/x", "/a/0", "/s/x", "a/b"] {
            assert!(apply_patches(&mut d, &[ProjectPatch::set(bad, json!(1))]).is_err(), "{bad}");
        }
        assert_eq!(d, json!({"a": [1, 2], "s": "x"}));
    }

    #[test]
    fn a_gap_asks_for_the_full_state_and_leaves_the_copy_alone() {
        let mut r = Replica::from_state(json!({"rev": 4, "x": 1}));
        assert_eq!(r.rev, 4);
        let err = r.apply(6, 7, &[ProjectPatch::set("/x", json!(2))]).unwrap_err();
        assert_eq!(err, ReplicaError::Gap { have: 4, base: 6 });
        assert_eq!(r.state["x"], 1);
        r.apply(4, 6, &[ProjectPatch::set("/x", json!(2)), ProjectPatch::set("/rev", json!(6))]).unwrap();
        assert_eq!((r.rev, r.state["x"].clone()), (6, json!(2)));
        // A bad path leaves the copy at its old revision too.
        assert!(r.apply(6, 7, &[ProjectPatch::set("/x", json!(3)), ProjectPatch::set("/nope/deeper", json!(1))]).is_err());
        assert_eq!((r.rev, r.state["x"].clone()), (6, json!(2)));
    }
}
