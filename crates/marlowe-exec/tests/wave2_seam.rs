//! The wave-2 groups (workflow, tasks, notify) are registered, in model-visible order after the
//! wave-1 groups; W's is filled, N's are empty until N fills its files. A track adds `ToolDef`s to its
//! own file and edits nothing else, so this pins what the seam promised.

use std::collections::HashSet;

use marlowe_agent::{ExposureProfile, ToolGroup, ToolHost};
use marlowe_exec::specs::{self, GROUPS};
use marlowe_exec::BuiltinTools;

const WAVE2: [ToolGroup; 3] = [ToolGroup::Workflow, ToolGroup::Tasks, ToolGroup::Notify];

#[test]
fn the_wave2_groups_follow_mcp_and_are_empty_until_their_tracks_land() {
    let order: Vec<ToolGroup> = GROUPS.iter().map(|g| g.group).collect();
    assert_eq!(order[order.len() - 3..], WAVE2, "{order:?}");
    assert!(order.iter().position(|g| *g == ToolGroup::Mcp) < order.iter().position(|g| *g == ToolGroup::Workflow));
    // W landed: its two tools, its block, offered in any workspace.
    let w = specs::group(ToolGroup::Workflow).unwrap();
    let names: Vec<String> = (w.tools)().into_iter().map(|d| d.spec.name).collect();
    assert_eq!(names, ["workflow", "report_findings"]);
    assert!((w.persona_block)().is_some() && (w.detect)(None));
    for group in [ToolGroup::Tasks, ToolGroup::Notify] {
        let g = specs::group(group).unwrap_or_else(|| panic!("{group:?} is not registered"));
        assert!((g.tools)().is_empty(), "{group:?}: its track has not landed, so it declares no tools");
        assert!((g.persona_block)().is_none(), "{group:?}: a block needs its capability live first");
        assert!(!(g.detect)(None));
        assert!(specs::table().defs.iter().all(|d| d.manifest.group != group));
    }
}

#[test]
fn every_group_but_the_daemons_has_a_registration() {
    for g in ToolGroup::ALL {
        let daemon_owned = matches!(g, ToolGroup::Memory | ToolGroup::Crew);
        assert_eq!(specs::group(g).is_some(), !daemon_owned, "{g:?}");
    }
    assert_eq!(ToolGroup::ALL.len(), 15);
    let names: HashSet<&str> = ToolGroup::ALL.iter().map(|g| g.as_str()).collect();
    assert_eq!(names.len(), ToolGroup::ALL.len(), "group names are unique");
    for w in WAVE2 {
        assert!(ToolGroup::ALL.contains(&w));
        assert_eq!(serde_json::from_str::<ToolGroup>(&format!("\"{}\"", w.as_str())).unwrap(), w);
    }
}

#[test]
fn a_frontier_session_exposes_the_wave2_groups_and_a_local_one_waits_for_detection() {
    let frontier = BuiltinTools::new(None).with_exposure(ExposureProfile::Frontier, specs::exposed_groups(ExposureProfile::Frontier, None));
    for g in WAVE2 {
        assert!(frontier.exposure().1.contains(&g), "{g:?}");
    }
    let local = BuiltinTools::new(None).with_exposure(ExposureProfile::Local, specs::exposed_groups(ExposureProfile::Local, None));
    assert!(local.exposure().1.contains(&ToolGroup::Workflow), "W: workflows are detected everywhere");
    for g in [ToolGroup::Tasks, ToolGroup::Notify] {
        assert!(!local.exposure().1.contains(&g), "{g:?}: not core, and nothing detects it yet");
    }
    // Adding the groups changed no existing tool's schema or position.
    let names: Vec<String> = frontier.specs().into_iter().map(|s| s.name).collect();
    assert!(names.iter().position(|n| n == "read") < names.iter().position(|n| n == "bash"));
}
