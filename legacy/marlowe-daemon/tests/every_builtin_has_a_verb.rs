//! **§B6's verb vocabulary is closed, and a BUILTIN missing from it is a gap in the closed set —
//! not the protection working.** ADR-059.
//!
//! `ToolCall::verb` is `&'static str` because the vocabulary is closed, so a verb arriving off the
//! wire is matched against a hand-written table and anything unrecognised renders as the generic
//! `tool`. There are TWO such tables — `project.rs`'s, for the main pane, and `watch_client.rs`'s,
//! for a `/watch` window — and **nothing bound either of them to `BUILTIN_TOOLS`**: a grep for that
//! constant returns 29 sites and not one is in `marlowe-daemon`.
//!
//! That is why it kept happening. `project.rs` already records the instance for `use` (*"seen live
//! 2026-08-26: two `use` calls in one turn showed as `... tool  release-notes`"*), and when this
//! file was written `watch_client.rs` was missing **`write` and `glob`** — both live builtins whose
//! calls reach the wire as `tool.to_string()`, so every one of them in a `/watch` window rendered
//! as `tool`. Those are the fourth and fifth instances of one gap.
//!
//! **This test failed on the tree as it stood.** That is its control, and it was a live one:
//!
//! ```text
//! ---- every_builtin_is_in_the_section_b6_verb_vocabulary stdout ----
//! assertion `left == right` failed: `write` is a builtin and the /watch window renders it as
//! the generic `tool`
//!   left: "tool"
//!  right: "write"
//! ```
//!
//! It pairs with `an_unrecognised_verb_does_not_leak_an_arbitrary_string_into_the_frame`, which
//! asserts the other direction: a verb that is NOT a builtin must still render as `tool`. Neither
//! is sufficient alone — one alone is satisfied by a table that maps everything, the other by a
//! table that maps nothing.

use marlowe_daemon::protocol::RunFrame;
use marlowe_daemon::watch_client::RunProjection;
use marlowe_daemon::{apply_events, view_from_status, Event, StatusReport};
use marlowe_tools::BUILTIN_TOOLS;
use marlowe_view::{Entry, ToolCall};

fn report() -> StatusReport {
    StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "marlowe-red:9b".into(),
        model_disclosure: "marlowe-red:9b".into(),
        degraded: None,
        rerank_provider: "cpu-sequential".into(),
        model_provider: "ollama".into(),
        live_runs: 0,
        models: Vec::new(),
        // Nothing has been announced into this fixture and nothing has been up.
        announcements: Vec::new(),
        uptime_ms: 0,
    }
}

/// The verb one `Event::Tool` renders to in the MAIN pane.
fn main_pane_verb(verb: &str) -> String {
    let mut v = view_from_status(&report());
    apply_events(
        &mut v,
        &[Event::Tool {
            id: 1,
            verb: verb.into(),
            target: "x".into(),
            state: "ok".into(),
            summary: "1 file".into(),
            detail: None,
        }],
    );
    let Some(Entry::Tools(calls)) = v.transcript.last() else {
        panic!("no tool group for `{verb}`");
    };
    calls[0].verb.to_string()
}

/// The verb one `RunFrame::Tool` renders to in a `/watch` WINDOW.
fn watch_window_verb(verb: &str) -> String {
    let mut p = RunProjection::new("a1b2c3d4e5f6");
    p.apply(&[
        Event::RunDetail {
            id: "a1b2c3d4e5f6".into(),
            status: "running".into(),
            parent: None,
            elapsed_ms: 1_000,
            spend_micros_usd: 0,
            ceiling_micros_usd: 1,
            spent_tokens: 0,
            granted_tokens: 1,
            depth: 0,
            last_checkpoint_step: None,
            resumable: false,
            orphan_policy: "detach".into(),
            pending_steers: 0,
            subagents: Vec::new(),
        },
        Event::RunOutput {
            seq: 1,
            frame: RunFrame::Tool {
                id: 1,
                verb: verb.into(),
                target: "x".into(),
                state: "ok".into(),
                summary: "1 file".into(),
            },
        },
    ]);
    let view = p.view().expect("a detail event was applied");
    let Some(Entry::Tools(calls)) = view.output.last() else {
        panic!("no tool group for `{verb}` in the window");
    };
    let calls: &Vec<ToolCall> = calls;
    calls[0].verb.to_string()
}

/// **Driven from `BUILTIN_TOOLS`, so adding a twelfth builtin fails here until both tables know
/// about it.** Typing the names would be a third copy of the list, and a third copy is how the
/// first two came to disagree.
#[test]
fn every_builtin_is_in_the_section_b6_verb_vocabulary() {
    for tool in BUILTIN_TOOLS {
        assert_eq!(
            main_pane_verb(tool),
            tool,
            "`{tool}` is a builtin and the main pane renders it as the generic `tool`"
        );
        assert_eq!(
            watch_window_verb(tool),
            tool,
            "`{tool}` is a builtin and the /watch window renders it as the generic `tool`"
        );
    }
}

/// A spawn reaches the wire as the verb `"spawn"` rather than as `run`, so it is not in
/// `BUILTIN_TOOLS` and the loop above cannot see it. Asserted separately rather than folded in,
/// because folding it in would mean the loop no longer reads a single constant.
#[test]
fn a_spawn_renders_as_a_verb_in_both_panes() {
    assert_eq!(main_pane_verb("spawn"), "spawn");
    assert_eq!(watch_window_verb("spawn"), "spawn");
}

/// **T15. An old journal's `find` still renders as a verb.**
///
/// The daemon seeds runs from the JOURNAL on restart, and `Event::Tool.verb` flows into the closed
/// vocabulary. `grep` was called `find` until ADR-059, so dropping the arm would render every
/// pre-rename tool line as the generic `tool` — a wall of them, in the pane a user goes to to read
/// what happened. One line in each table, kept deliberately.
///
/// Control, run: deleting `"find" => "find",` from `project.rs` fails this with
/// `left: "tool", right: "find"`.
#[test]
fn an_old_journals_find_still_renders_as_a_verb() {
    assert_eq!(main_pane_verb("find"), "find", "a pre-ADR-059 journal must stay readable");
    assert_eq!(watch_window_verb("find"), "find", "and in the window too");
}

/// The other direction, in the window this time — `project.rs` has its own copy of this and the
/// window did not. A table that maps everything would satisfy the tests above and destroy the
/// property the closed vocabulary exists for.
#[test]
fn an_unrecognised_verb_still_renders_as_tool_in_the_window() {
    assert_eq!(watch_window_verb("definitely-not-a-builtin"), "tool");
}
