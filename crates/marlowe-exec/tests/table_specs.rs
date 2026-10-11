//! The registration table declares exactly the tools M4.5 declared, byte for byte, in the same
//! order, except where M4.6 replaced a group with a reviewed surface of its own. `fixtures/specs_m45.rs`
//! is the old `specs.rs`, kept verbatim and never regenerated: a schema or description change must
//! show up here as a deliberate fixture edit.
//!
//! The files group (track A) replaced `read`/`write`/`edit`: its surface is pinned in
//! `src/specs/files/specs_m46_files.json` (`the_files_group_declares_the_reviewed_m46_schemas`), and
//! here it must sit where the M4.5 file tools sat. The groups M4.6 added (code, verify, agent, git)
//! pin their own lists in their own tests.

use std::sync::Arc;

use marlowe_agent::{ToolGroup, ToolHost};
use marlowe_exec::{BuiltinTools, SearchResponse};
use marlowe_model::{CancelToken, ToolSpec};

#[allow(dead_code)]
#[path = "fixtures/specs_m45.rs"]
mod specs_m45;

const M45_FILES: [&str; 3] = ["read", "write", "edit"];

/// The groups M4.5 had, as the host declares them.
fn m45_groups(t: &BuiltinTools) -> Vec<ToolSpec> {
    t.specs()
        .into_iter()
        .filter(|s| {
            t.manifest(&s.name)
                .is_some_and(|m| {
                    !matches!(
                        m.group,
                        ToolGroup::Verify
                            | ToolGroup::Code
                            | ToolGroup::Agent
                            | ToolGroup::Git
                            | ToolGroup::Workflow
                            | ToolGroup::Tasks
                            | ToolGroup::Notify
                    )
                })
        })
        .collect()
}

/// The M4.5 declaration with its file tools replaced, in place, by the reviewed M4.6 files group
/// (`edit_lines` is off unless a host asks for it).
fn expected(with_search: bool) -> Vec<ToolSpec> {
    let files: Vec<ToolSpec> =
        serde_json::from_str::<Vec<ToolSpec>>(include_str!("../src/specs/files/specs_m46_files.json"))
            .unwrap()
            .into_iter()
            .filter(|s| s.name != "edit_lines")
            .collect();
    let old = specs_m45::all(with_search);
    let at = old.iter().position(|s| M45_FILES.contains(&s.name.as_str())).expect("M4.5 declared the file tools");
    let mut out: Vec<ToolSpec> = old.iter().take(at).cloned().collect();
    out.extend(files);
    out.extend(old.into_iter().skip(at).filter(|s| !M45_FILES.contains(&s.name.as_str())));
    out
}

#[test]
fn the_table_declares_the_m45_tools_unchanged() {
    assert_eq!(m45_groups(&BuiltinTools::new(None)), expected(false));
    let search = |_: &str, _: &CancelToken| -> Result<SearchResponse, String> { Err("offline".into()) };
    let with_search = BuiltinTools::new(None).with_search(Arc::new(search));
    assert_eq!(m45_groups(&with_search), expected(true));
}
