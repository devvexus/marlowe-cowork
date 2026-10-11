//! The registration table declares exactly the tools M4.5 declared, byte for byte, in the same
//! order. `fixtures/specs_m45.rs` is the old `specs.rs`, kept verbatim and never regenerated: a
//! schema or description change must show up here as a deliberate fixture edit.

use std::sync::Arc;

use marlowe_agent::{ToolGroup, ToolHost};
use marlowe_exec::{BuiltinTools, SearchResponse};
use marlowe_model::CancelToken;

#[allow(dead_code)]
#[path = "fixtures/specs_m45.rs"]
mod specs_m45;

/// The tools M4.5 declared: the host's specs without the verify group (track D adds it; its own
/// tests pin its schemas).
fn m45(t: &BuiltinTools) -> Vec<marlowe_model::ToolSpec> {
    t.specs().into_iter().filter(|s| t.manifest(&s.name).is_some_and(|m| m.group != ToolGroup::Verify)).collect()
}

#[test]
fn the_table_declares_the_m45_tools_unchanged() {
    assert_eq!(m45(&BuiltinTools::new(None)), specs_m45::all(false));
    let search = |_: &str, _: &CancelToken| -> Result<SearchResponse, String> { Err("offline".into()) };
    let with_search = BuiltinTools::new(None).with_search(Arc::new(search));
    assert_eq!(m45(&with_search), specs_m45::all(true));
}
