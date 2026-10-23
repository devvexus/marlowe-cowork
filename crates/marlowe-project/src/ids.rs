//! Id and clock helpers. Ids come from content wherever the content is known (a source's identity,
//! a claim's quote offsets), so replaying a run in a different arrival order yields the same ids.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A fresh project id: `p_` and 8 hex characters.
pub fn new_project_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("p_{}", &u[..8])
}

/// Whether `id` is shaped like a project id (`p_` and 8 lower-case hex characters). The store
/// refuses any other string before it builds a path from it.
pub fn is_project_id(id: &str) -> bool {
    id.len() == 10 && id.starts_with("p_") && id[2..].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A content-derived id: `<prefix>_` and the first 8 hex characters of blake3 over the parts
/// (length-prefixed, so `("ab","c")` and `("a","bc")` differ).
pub fn derived_id(prefix: &str, parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    format!("{prefix}_{}", &h.finalize().to_hex()[..8])
}

/// An item id (`i_<8hex>`) from the project, the item's kind and its key.
pub fn item_id(project: &str, kind: &str, key: &str) -> String {
    derived_id("i", &[project, kind, key])
}

/// The id of agent number `n` (`a_<n>`).
pub fn agent_id(n: u32) -> String {
    format!("a_{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_ids_have_the_shape_the_store_checks() {
        for _ in 0..50 {
            assert!(is_project_id(&new_project_id()));
        }
        for bad in ["", "p_", "p_7F3A91C2", "p_7f3a91c", "p_7f3a91c22", "../p_7f3a91c2", "q_7f3a91c2", "p_7f3a91c/"] {
            assert!(!is_project_id(bad), "{bad}");
        }
    }

    #[test]
    fn derived_ids_depend_on_content_only_and_parts_do_not_run_together() {
        assert_eq!(item_id("p_1", "source", "k"), item_id("p_1", "source", "k"));
        assert_ne!(item_id("p_1", "source", "k"), item_id("p_1", "source", "j"));
        assert_ne!(derived_id("i", &["ab", "c"]), derived_id("i", &["a", "bc"]));
        assert!(item_id("p_1", "source", "k").starts_with("i_"));
        assert_eq!(item_id("p_1", "source", "k").len(), 10);
    }
}
