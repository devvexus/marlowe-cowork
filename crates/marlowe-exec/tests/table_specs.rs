//! The registration table declares the reviewed compact surface (`marlowe-agent/src/compact_specs.json`),
//! byte for byte, and the pinned fixtures say so independently of the code. A schema or description
//! change must show up here as a deliberate fixture edit:
//!
//! - `src/specs/files/specs_m46_files.json`: read, write, edit, multi_edit, edit_lines, notebook_edit;
//! - `src/specs/specs_m46_web.json`: web_fetch, web_search;
//! - `src/specs/specs_a15.json`: glob, grep, bash, powershell, python, arxiv.
//!
//! The other groups (code, verify, agent, git, workflow, tasks, notify) pin themselves against the
//! compact file here. The 2026-10-12 tool overhaul regenerated all three fixtures from the new specs
//! (merged and renamed tools, one-line descriptions, typeless plain strings).

use std::sync::Arc;

use marlowe_agent::{compact_specs, ToolGroup, ToolHost};
use marlowe_exec::{BuiltinTools, SearchResponse};
use marlowe_model::{CancelToken, ToolSpec};

fn pinned() -> Vec<ToolSpec> {
    let mut out: Vec<ToolSpec> = Vec::new();
    for json in [
        include_str!("../src/specs/files/specs_m46_files.json"),
        include_str!("../src/specs/specs_m46_web.json"),
        include_str!("../src/specs/specs_a15.json"),
    ] {
        out.extend(serde_json::from_str::<Vec<ToolSpec>>(json).unwrap());
    }
    out
}

/// A host that exposes every group and has a search backend.
fn host() -> BuiltinTools {
    let search = |_: &str, _: &CancelToken| -> Result<SearchResponse, String> { Err("offline".into()) };
    BuiltinTools::new(None).with_search(Arc::new(search))
}

#[test]
fn the_pinned_fixtures_are_the_table_and_the_compact_file() {
    let t = host();
    let pins = pinned();
    assert_eq!(pins.len(), 14);
    for p in &pins {
        let def = t.tool(&p.name).unwrap_or_else(|| panic!("`{}` is pinned but not registered", p.name));
        assert_eq!(&def.spec, p, "{} differs from its fixture", p.name);
        assert_eq!(&compact_specs::spec(&p.name), p, "{} differs from compact_specs.json", p.name);
    }
}

#[test]
fn every_registered_tool_is_in_the_compact_file_unchanged() {
    for def in marlowe_exec::specs::table().defs.iter() {
        assert_eq!(def.spec, compact_specs::spec(def.name()), "{}", def.name());
    }
}

#[test]
fn every_compact_spec_is_registered_or_belongs_to_the_daemon() {
    // Declared by other crates: the memory and crew tools, MCP resources, the subagent-only tools
    // and a workflow agent's `structured_output`.
    const ELSEWHERE: [&str; 11] = [
        "remember", "recall", "forget", "correct", "dispatch", "list_mcp_resources", "read_mcp_resource", "handback",
        "set_statusline", "create_agent", "structured_output",
    ];
    let table = marlowe_exec::specs::table();
    for s in compact_specs::all() {
        let here = table.get(&s.name).is_some();
        assert_ne!(here, ELSEWHERE.contains(&s.name.as_str()), "`{}` is {}", s.name, if here { "registered here and listed as elsewhere" } else { "neither registered nor listed as elsewhere" });
    }
}

#[test]
fn a_full_host_offers_the_registered_tools_in_group_order() {
    let t = host().with_exposure(marlowe_agent::ExposureProfile::Frontier, ToolGroup::ALL.to_vec());
    let names: Vec<String> = t.specs().into_iter().map(|s| s.name).collect();
    // Hosts that are not configured hide the groups that need the daemon (agent, git, tasks...);
    // what remains keeps the table's order.
    let table_order: Vec<&str> = marlowe_exec::specs::table().defs.iter().map(|d| d.name()).collect();
    let mut at = 0;
    for n in &names {
        let i = table_order.iter().position(|t| t == n).unwrap_or_else(|| panic!("`{n}` is not in the table"));
        assert!(i >= at, "`{n}` is out of order");
        at = i;
    }
    assert!(names.contains(&"read".to_string()) && names.contains(&"web_search".to_string()));
}
