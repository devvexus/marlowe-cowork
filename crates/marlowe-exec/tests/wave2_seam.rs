//! The wave-2 groups (workflow, tasks, notify) are registered, in model-visible order after the
//! wave-1 groups, and empty until tracks W and N fill their files. A track adds `ToolDef`s to its
//! own file and edits nothing else, so this pins what the seam promised.

use std::collections::HashSet;

use marlowe_agent::{ExposureProfile, ToolGroup, ToolHost};
use marlowe_exec::specs::{self, GROUPS};
use marlowe_exec::BuiltinTools;

const WAVE2: [ToolGroup; 3] = [ToolGroup::Workflow, ToolGroup::Tasks, ToolGroup::Notify];

#[test]
fn the_wave2_groups_follow_mcp_and_each_declares_only_what_its_track_landed() {
    let order: Vec<ToolGroup> = GROUPS.iter().map(|g| g.group).collect();
    assert_eq!(order[order.len() - 3..], WAVE2, "{order:?}");
    assert!(order.iter().position(|g| *g == ToolGroup::Mcp) < order.iter().position(|g| *g == ToolGroup::Workflow));
    // Workflow (track W) has not landed here: no tools, no block, nothing to detect.
    let w = specs::group(ToolGroup::Workflow).expect("registered");
    assert!((w.tools)().is_empty(), "workflow: its track has not landed, so it declares no tools");
    assert!((w.persona_block)().is_none(), "a block needs its capability live first");
    assert!(!(w.detect)(None));
    assert!(specs::table().defs.iter().all(|d| d.manifest.group != ToolGroup::Workflow));
    // Tasks and notify (track N) have: their tools, a block with the tasks group, and detection
    // wherever there is a workspace.
    let names = |g: ToolGroup| -> Vec<String> { (specs::group(g).unwrap().tools)().iter().map(|d| d.name().to_string()).collect() };
    assert_eq!(
        names(ToolGroup::Tasks),
        ["task_list", "task_output", "task_stop", "monitor", "cron_create", "cron_list", "cron_delete", "schedule_wakeup", "list_agents", "watch"]
    );
    assert_eq!(names(ToolGroup::Notify), ["notify", "send_file", "notices"]);
    let t = specs::group(ToolGroup::Tasks).unwrap();
    assert!((t.persona_block)().is_some_and(|b| b.contains("<background_work>")));
    for g in [ToolGroup::Tasks, ToolGroup::Notify] {
        let g = specs::group(g).unwrap();
        assert!(!(g.detect)(None) && (g.detect)(Some(std::path::Path::new("C:/work/valiance"))));
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
    for g in WAVE2 {
        assert!(!local.exposure().1.contains(&g), "{g:?}: not core, and there is no workspace to detect it in");
    }
    // Adding the groups changed no existing tool's schema or position.
    let names: Vec<String> = frontier.specs().into_iter().map(|s| s.name).collect();
    assert!(names.iter().position(|n| n == "read") < names.iter().position(|n| n == "bash"));
}
