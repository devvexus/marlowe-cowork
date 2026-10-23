//! The wave-2 groups (workflow, tasks, notify) are registered, in model-visible order after the
//! wave-1 groups; W's is filled, N's are empty until N fills its files. A track adds `ToolDef`s to its
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
    // W landed: its two tools, its block, offered in any workspace.
    let w = specs::group(ToolGroup::Workflow).unwrap();
    let names = |g: ToolGroup| -> Vec<String> { (specs::group(g).unwrap().tools)().iter().map(|d| d.name().to_string()).collect() };
    assert_eq!(names(ToolGroup::Workflow), ["team", "report_findings"]);
    assert!((w.persona_block)().is_none() && (w.detect)(None));
    // Tasks and notify (track N): their tools, a block with the tasks group, and detection
    // wherever there is a workspace.
    assert_eq!(
        names(ToolGroup::Tasks),
        ["task_list", "task_output", "task_stop", "monitor", "schedule", "watch"]
    );
    assert_eq!(names(ToolGroup::Notify), ["notify", "send_file", "notices"]);
    let t = specs::group(ToolGroup::Tasks).unwrap();
    assert!((t.persona_block)().is_none(), "the tools say what they are for; no block");
    let tasks = specs::group(ToolGroup::Tasks).unwrap();
    assert!((tasks.detect)(None), "the background-work notice names task_list, so it is there with no workspace too");
    let notify = specs::group(ToolGroup::Notify).unwrap();
    assert!((notify.detect)(None) && (notify.detect)(Some(std::path::Path::new("C:/work/valiance"))), "a conversation with no workspace can still be sent a file");
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
    assert!(local.exposure().1.contains(&ToolGroup::Tasks), "the notice names task_list: offered with no workspace");
    assert!(local.exposure().1.contains(&ToolGroup::Notify), "notify: offered with no workspace, so a finished run's file can be sent");
    // Adding the groups changed no existing tool's schema or position.
    let names: Vec<String> = frontier.specs().into_iter().map(|s| s.name).collect();
    assert!(names.iter().position(|n| n == "read") < names.iter().position(|n| n == "bash"));
}
