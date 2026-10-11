//! The crew in the client: the Crew board section, the Run view (research report, build diff and merge),
//! the ^d dispatch prompt, toasts, steering and the run approvals.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_model::Effort;
use marlowe_proto::{
    ApprovalId, Citation, Confidence, CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, CrewStep, DiffSummary,
    Dispatch, Event, Playbook, Request, RoleInfo, RunId, SandboxInfo, SandboxKind, Slot,
};
use marlowe_tui::crew::{RunConfirm, RUN_LIMIT};
use marlowe_tui::state::Overlay;
use marlowe_tui::{State, View};

const NOW: i64 = 1_800_000_000_000;

fn run(id: &str, role: &str, title: &str, status: CrewRunStatus) -> CrewRunInfo {
    CrewRunInfo {
        id: RunId::from(id),
        session: Some(sid()),
        role: role.into(),
        title: title.into(),
        goal: title.into(),
        playbook: None,
        status,
        workspace: Some("C:/work/valiance".into()),
        sandbox: None,
        budget: CrewBudget { tokens: Some(400_000), steps: Some(40), usd: Some(0.5) },
        spent: CrewSpent::default(),
        step: 0,
        current_action: None,
        note: None,
        tainted: false,
        created_ms: NOW - 20 * 60_000,
        updated_ms: NOW - 60_000,
        result: None,
        workflow: None,
    }
}

fn research_run() -> CrewRunInfo {
    let mut r = run("aa11bb22cc33dd44", "researcher", "LoRA rank ablations for 22B gates", CrewRunStatus::Running);
    r.goal = "What does the literature say about LoRA rank for gating adapters at 22B?".into();
    r.playbook = Some(Playbook::DeepResearch);
    r.sandbox = Some(SandboxInfo {
        kind: SandboxKind::Scratch,
        root: "C:/Users/m/AppData/Local/MarloweCowork/scratch/aa11bb22cc33dd44".into(),
        origin: None,
        branch: None,
    });
    r.step = 6;
    r.spent = CrewSpent { tokens: 41_200, steps: 6, usd: 0.0213 };
    r.current_action = Some("scout 2/4 · web_search \"lora rank ablation\"".into());
    r
}

fn build_run() -> CrewRunInfo {
    let mut r = run("ee55ff66aa77bb88", "engineer", "fix the gate threshold off-by-one", CrewRunStatus::NeedsYou);
    r.playbook = Some(Playbook::Build);
    r.sandbox = Some(SandboxInfo {
        kind: SandboxKind::Worktree,
        root: "C:/Users/m/AppData/Local/MarloweCowork/worktrees/ee55ff66aa77bb88".into(),
        origin: Some("C:/work/valiance".into()),
        branch: Some("crew/ee55ff66".into()),
    });
    r.step = 14;
    r.spent = CrewSpent { tokens: 88_000, steps: 14, usd: 0.1840 };
    r.note = Some("tests pass (412) — merge crew/ee55ff66 into main?".into());
    r.result = Some(Dispatch {
        summary: "Fixed the off-by-one in `gate::threshold`: the sweep now includes its upper bound. \
                  Added a regression test; the full suite passes."
            .into(),
        artifacts: vec![],
        confidence: Confidence::High,
        citations: vec![],
        diff: Some(DiffSummary {
            branch: "crew/ee55ff66".into(),
            files: vec!["src/gate/threshold.rs".into(), "tests/gate.rs".into()],
            insertions: 31,
            deletions: 4,
        }),
    });
    r
}

fn mixed_runs() -> Vec<CrewRunInfo> {
    let mut failed = run("cc99dd00", "analyst", "eval drift between wnr4 and wnr5", CrewRunStatus::Failed);
    failed.note = Some("budget: steps reached 40".into());
    failed.updated_ms = NOW - 2 * 3_600_000;
    let mut done = run("dd11ee22", "reviewer", "review the sentinel heartbeat parser", CrewRunStatus::Done);
    done.updated_ms = NOW - 30 * 60_000;
    done.result = Some(Dispatch { summary: "Two blocking issues, both in the stall timer.".into(), ..Default::default() });
    let queued = run("ff33aa44", "scout", "find the wnr3 checkpoint notes", CrewRunStatus::Queued);
    vec![done, research_run(), failed, build_run(), queued]
}

fn roles() -> Vec<RoleInfo> {
    let role = |name: &str, description: &str| RoleInfo {
        name: name.into(),
        description: description.into(),
        slot: Slot::Crew(name.into()),
        effort: Effort::Medium,
        tools: vec!["read".into(), "grep".into()],
        budget: CrewBudget { tokens: Some(400_000), steps: Some(40), usd: Some(0.5) },
        sandbox: SandboxKind::Scratch,
        user_file: None,
    };
    vec![
        role("analyst", "Reads numbers and logs; says what changed and why it matters."),
        role("engineer", "Changes code in a worktree, runs the tests, leaves a diff."),
        role("researcher", "Plans a question, sends scouts, writes a cited report."),
        role("reviewer", "Tries to break a claim or a diff; lists what blocks it."),
        role("scout", "Answers one narrow question with sources."),
    ]
}

/// Connected, a model, an open session, the rail fed with `runs`.
fn with_runs(runs: Vec<CrewRunInfo>) -> State {
    let mut s = state();
    s.date = "Friday 9 October".into();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "gate work".into(), workspace: Some("C:/work/valiance".into()) });
    s.session = Some(sid());
    feed(&mut s, 0, Event::Runs { list: runs });
    s
}

#[test]
fn welcome_asks_for_runs_and_a_missing_crew_is_quiet() {
    let mut s = state();
    let reqs = welcome(&mut s, 0);
    assert!(reqs.contains(&Request::ListRuns { limit: RUN_LIMIT }));
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::crew::NO_CREW.into() });
    assert!(s.flash.is_none(), "the probe's answer is not flashed");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "crew not running"));
    // A user action that meets the same answer is told.
    s.on_key(ctrl('d'));
    feed(&mut s, 0, Event::Roles { list: roles() });
    type_str(&mut s, "x");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::crew::NO_CREW.into() });
    assert_eq!(s.flash.as_ref().map(|f| f.text.as_str()), Some(marlowe_tui::crew::NO_CREW));
}

#[test]
fn rail_shows_mixed_run_states_most_urgent_first() {
    let s = with_runs(mixed_runs());
    let order: Vec<&str> = s.crew.ordered().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(order, ["ee55ff66aa77bb88", "aa11bb22cc33dd44", "ff33aa44", "dd11ee22", "cc99dd00"]);
    let buf = render(&s, W, H);
    for want in [
        "Crew",
        "1 needs you",
        "◐ engineer · fix the gate thre",
        "tests pass (412)",
        "● researcher · LoRA rank ablat",
        "step 6 · $0.0213 · scout 2/4",
        "○ scout · find the wnr3",
        "queued",
        "+2 more · alt-r",
    ] {
        assert!(contains(&buf, want), "rail is missing {want:?}\n{}", dump(&buf));
    }
    assert_palette_only(&buf);
    snapshot("crew_rail", &buf);

    // The list (esc out of a run view) shows every run with its status word.
    let mut s = s;
    s.on_key(alt('r'));
    assert_eq!(s.view, View::Crew);
    s.on_key(key(KeyCode::Esc));
    assert!(s.crew.open.is_none());
    let buf = render(&s, W, H);
    for want in ["Crew", "5 runs · 1 need you · 1 running", "✓ reviewer", "Two blocking issues", "✗ analyst", "budget: steps reached 40"] {
        assert!(contains(&buf, want), "list is missing {want:?}\n{}", dump(&buf));
    }
    assert_palette_only(&buf);
}

#[test]
fn ctrl_r_opens_the_most_urgent_run_and_streams_its_steps() {
    let mut s = with_runs(vec![research_run()]);
    let reqs = s.on_key(alt('r'));
    let id = RunId::from("aa11bb22cc33dd44");
    assert_eq!(reqs, vec![Request::ListRuns { limit: RUN_LIMIT }, Request::WatchRun { run: id.clone() }]);
    assert_eq!(s.crew.open.as_ref().map(|v| &v.run), Some(&id));
    feed(&mut s, 0, Event::RunTranscript {
        run: research_run(),
        steps: vec![CrewStep { step: 1, role: "researcher".into(), action: "plan".into(), text: "1. ranks tried\n2. gate placement".into(), at_ms: NOW - 600_000 }],
    });
    feed(&mut s, 5, Event::RunStep { run: id.clone(), step: 2, action: "scout 1/4 · arxiv \"lora rank\"".into(), text_delta: Some("Found 3 papers".into()) });
    feed(&mut s, 6, Event::RunStep { run: id.clone(), step: 2, action: "scout 1/4 · paper 2106.09685".into(), text_delta: Some(" on rank.".into()) });
    let v = s.crew.open.as_ref().unwrap();
    assert!(v.loaded);
    assert_eq!(v.steps.len(), 2);
    assert_eq!(v.steps[1].text, "Found 3 papers on rank.");
    assert_eq!(s.crew.get(&id).unwrap().current_action.as_deref(), Some("scout 1/4 · paper 2106.09685"));
    let buf = render(&s, W, H);
    for want in ["researcher · LoRA rank ablations", "running", "step 6 of 40", "$0.0213 of $0.50", "TRANSCRIPT", "plan", "Found 3 papers on rank.", "s to steer"] {
        assert!(contains(&buf, want), "run view is missing {want:?}\n{}", dump(&buf));
    }
    assert_palette_only(&buf);

    // Steer: s focuses the box, Enter sends; the note shows in the transcript at once.
    s.on_key(key(KeyCode::Char('s')));
    type_str(&mut s, "skip vision papers");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::Steer { run: id.clone(), text: "skip vision papers".into() }]);
    assert!(contains(&render(&s, W, H), "skip vision papers"));
    // p pauses, c asks first.
    assert_eq!(s.on_key(key(KeyCode::Char('p'))), vec![Request::PauseRun { run: id.clone() }]);
    assert!(s.on_key(key(KeyCode::Char('c'))).is_empty());
    assert_eq!(s.crew.open.as_ref().unwrap().confirm, Some(RunConfirm::Cancel));
    assert!(contains(&render(&s, W, H), "cancel this run?"));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::CancelRun { run: id }]);
}

#[test]
fn research_run_view_renders_the_report() {
    let dir = std::env::temp_dir().join(format!("marlowe-tui-crew-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let report = dir.join("report.md");
    std::fs::write(
        &report,
        "# LoRA rank for gating adapters\n\n**Rank 8 is enough** for gates at 22B; rank 64 buys nothing \
         measurable [1].\n\n## Evidence\n\n- Hu et al. find rank 4–8 matches full fine-tuning [1].\n- \
         One contested result at rank 2 was dropped by the reviewer.\n",
    )
    .unwrap();
    let mut r = research_run();
    r.status = CrewRunStatus::NeedsYou;
    r.current_action = None;
    r.note = Some("report ready — accept it into research/?".into());
    r.result = Some(Dispatch {
        summary: "Rank 8 is enough for gating adapters at 22B.".into(),
        artifacts: vec![report.to_string_lossy().into_owned()],
        confidence: Confidence::High,
        citations: vec![
            Citation { title: "LoRA: Low-Rank Adaptation of Large Language Models".into(), url_or_path: "https://arxiv.org/abs/2106.09685".into() },
            Citation { title: "valiance gate notes".into(), url_or_path: "docs/gate.md".into() },
        ],
        diff: None,
    });
    let mut s = with_runs(vec![r.clone()]);
    s.on_key(alt('r'));
    feed(&mut s, 0, Event::RunTranscript { run: r.clone(), steps: vec![
        CrewStep { step: 1, role: "researcher".into(), action: "plan · 3 sub-questions".into(), text: String::new(), at_ms: NOW - 900_000 },
        CrewStep { step: 9, role: "researcher".into(), action: "synthesize · report.md".into(), text: "(written)".into(), at_ms: NOW - 60_000 },
    ] });
    assert!(s.crew.open.as_ref().unwrap().report.as_deref().is_some_and(|t| t.contains("Rank 8 is enough")));
    let buf = render(&s, W, H);
    for want in [
        "REPORT",
        "confidence high · 2 sources",
        "LoRA rank for gating adapters",
        "Rank 8 is enough",
        "SOURCES",
        "https://arxiv.org/abs/2106.09685",
        "accept: copy the report into C:/work/valiance/research",
        "m accept report",
    ] {
        assert!(contains(&buf, want), "research view is missing {want:?}\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "# LoRA"), "the report is rendered, not shown raw");
    assert_palette_only(&buf);
    let _ = std::fs::remove_dir_all(&dir);
    snapshot("run_research", &buf);
    // m accepts through the daemon (which asks for approval).
    let reqs = s.on_key(key(KeyCode::Char('m')));
    assert_eq!(reqs, vec![Request::MergeRun { run: r.id }]);
}

#[test]
fn build_run_view_shows_the_diff_and_merges_only_on_request() {
    let mut s = with_runs(vec![build_run()]);
    s.on_key(alt('r'));
    let id = build_run().id;
    feed(&mut s, 0, Event::RunTranscript { run: build_run(), steps: vec![
        CrewStep { step: 1, role: "engineer".into(), action: "plan".into(), text: "Off-by-one in the sweep bound.".into(), at_ms: NOW - 900_000 },
        CrewStep { step: 11, role: "engineer".into(), action: "test · cargo test (pass, 412)".into(), text: String::new(), at_ms: NOW - 200_000 },
        CrewStep { step: 13, role: "reviewer".into(), action: "review · no blocking issues".into(), text: String::new(), at_ms: NOW - 100_000 },
    ] });
    let buf = render(&s, W, H);
    for want in [
        "engineer · fix the gate threshold off-by-one",
        "needs you",
        "worktree crew/ee55ff66 of C:/work/valiance",
        "DIFF",
        "crew/ee55ff66 · 2 files · +31 −4",
        "src/gate/threshold.rs",
        "m merge crew/ee55ff66 into C:/work/valiance, after your approval; nothing is pushed",
        "d discard",
        "review · no blocking issues",
    ] {
        assert!(contains(&buf, want), "build view is missing {want:?}\n{}", dump(&buf));
    }
    assert_palette_only(&buf);
    snapshot("run_build", &buf);

    // m sends MergeRun; the daemon's approval (attributed to the run) is what applies it.
    assert_eq!(s.on_key(key(KeyCode::Char('m'))), vec![Request::MergeRun { run: id.clone() }]);
    feed(&mut s, 9, Event::ApprovalRequested {
        approval: ApprovalId(7),
        session: sid(),
        tool: "merge".into(),
        summary: "merge crew/ee55ff66 into main (fast-forward)".into(),
        detail: String::new(),
        run: Some(id.clone()),
        proposed_rule: None,
    });
    let buf = render(&s, W, H);
    assert!(contains(&buf, "from the crew · engineer · fix the gate threshold off-by-one"));
    assert_palette_only(&buf);
    s.now_ms += 1_000;
    s.on_key(key(KeyCode::Char('n')));

    // d asks first; n keeps it, y discards.
    assert!(s.on_key(key(KeyCode::Char('d'))).is_empty());
    assert!(contains(&render(&s, W, H), "the worktree and its branch are deleted"));
    assert!(s.on_key(key(KeyCode::Char('n'))).is_empty());
    s.on_key(key(KeyCode::Char('d')));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::DiscardRun { run: id }]);
}

#[test]
fn dispatch_prompt_picks_a_role_a_goal_and_a_playbook() {
    let mut s = with_runs(vec![]);
    let reqs = s.on_key(ctrl('d'));
    assert_eq!(reqs, vec![Request::ListRoles]);
    assert_eq!(s.overlay, Some(Overlay::Dispatch));
    assert!(contains(&render(&s, W, H), "loading roles…"));
    feed(&mut s, 0, Event::Roles { list: roles() });
    // Re-opened with roles known, the researcher is preselected.
    s.on_key(key(KeyCode::Esc));
    assert!(s.overlay.is_none());
    s.on_key(ctrl('d'));
    assert_eq!(s.crew.prompt.as_ref().unwrap().role, 2);
    type_str(&mut s, "compare LoRA ranks for the 22B gates");
    s.on_key(key(KeyCode::Tab));
    let buf = render(&s, W, H);
    for want in ["dispatch", "ROLE", "› researcher", "GOAL", "compare LoRA ranks", "PLAYBOOK", "deep_research", "plan → scouts in parallel → verify → a cited report"] {
        assert!(contains(&buf, want), "prompt is missing {want:?}\n{}", dump(&buf));
    }
    assert_palette_only(&buf);
    snapshot("dispatch_prompt", &buf);

    // Down to the reviewer; three Tabs go round (build, none) and back to deep_research.
    s.on_key(key(KeyCode::Down));
    s.on_key(key(KeyCode::Tab));
    s.on_key(key(KeyCode::Tab));
    s.on_key(key(KeyCode::Tab));
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(
        reqs,
        vec![Request::Dispatch {
            session: Some(sid()),
            role: "reviewer".into(),
            goal: "compare LoRA ranks for the 22B gates".into(),
            playbook: Some(Playbook::DeepResearch),
            workspace: Some("C:/work/valiance".into()),
            budget: None,
        }]
    );
    assert!(s.overlay.is_none());
    assert!(s.flash.as_ref().is_some_and(|f| f.text.contains("dispatched reviewer")));

    // An empty goal is not sent.
    s.on_key(ctrl('d'));
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    assert_eq!(s.overlay, Some(Overlay::Dispatch));
}

#[test]
fn a_run_that_needs_you_toasts_once() {
    let mut s = with_runs(vec![research_run()]);
    let mut r = build_run();
    r.status = CrewRunStatus::Running;
    r.note = None;
    feed(&mut s, 3, Event::RunUpdated(r.clone()));
    assert!(s.crew.toasts.is_empty(), "running is not news");
    feed(&mut s, 4, Event::RunUpdated(build_run()));
    assert_eq!(s.crew.toasts.len(), 1);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "◐ engineer needs you · fix the gate"));
    assert!(contains(&buf, "tests pass (412)"));
    assert_palette_only(&buf);
    snapshot("crew_toast", &buf);
    // The same state again does not toast twice; opening the run clears its toast.
    feed(&mut s, 5, Event::RunUpdated(build_run()));
    assert_eq!(s.crew.toasts.len(), 1);
    s.on_key(alt('r'));
    assert!(s.crew.toasts.is_empty());
    // A finish toasts green; the result is kept when a later update carries none.
    let mut done = research_run();
    done.status = CrewRunStatus::Done;
    feed(&mut s, 6, Event::RunFinished { run: done.id.clone(), dispatch: Dispatch { summary: "Rank 8 is enough.".into(), ..Default::default() } });
    feed(&mut s, 7, Event::RunUpdated(done.clone()));
    assert!(s.crew.toasts.iter().any(|t| t.title.contains("researcher finished")));
    assert!(s.crew.get(&done.id).unwrap().result.is_some());
    s.now_ms += marlowe_tui::wake::TOAST_MS + 1;
    assert_eq!(s.crew.live_toasts(s.now_ms).count(), 0);
}

#[test]
fn ctrl_d_stays_forget_in_the_memory_view_and_palette_has_crew() {
    let mut s = with_runs(vec![]);
    s.on_key(ctrl('k'));
    type_str(&mut s, "dispatch");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::ListRoles]);
    assert_eq!(s.overlay, Some(Overlay::Dispatch));
    s.on_key(key(KeyCode::Esc));
    s.on_key(ctrl('k'));
    type_str(&mut s, "crew");
    s.on_key(key(KeyCode::Enter));
    assert_eq!(s.view, View::Crew);
    assert!(contains(&render(&s, W, H), "No runs yet. ^d dispatches one"));
    s.on_key(ctrl('g'));
    assert_eq!(s.view, View::Memory);
    s.on_key(ctrl('d'));
    assert!(s.overlay.is_none(), "^d in the memory view is forget, not dispatch");
}
