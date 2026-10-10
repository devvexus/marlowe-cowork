//! The registration table declares exactly the tools M4.5 declared, byte for byte, in the same
//! order. `fixtures/specs_m45.rs` is the old `specs.rs`, kept verbatim and never regenerated: a
//! schema or description change must show up here as a deliberate fixture edit.

use std::sync::Arc;

use marlowe_agent::ToolHost;
use marlowe_exec::{BuiltinTools, SearchResponse};
use marlowe_model::CancelToken;

#[allow(dead_code)]
#[path = "fixtures/specs_m45.rs"]
mod specs_m45;

#[test]
fn the_table_declares_the_m45_tools_unchanged() {
    assert_eq!(BuiltinTools::new(None).specs(), specs_m45::all(false));
    let search = |_: &str, _: &CancelToken| -> Result<SearchResponse, String> { Err("offline".into()) };
    let with_search = BuiltinTools::new(None).with_search(Arc::new(search));
    assert_eq!(with_search.specs(), specs_m45::all(true));
}
