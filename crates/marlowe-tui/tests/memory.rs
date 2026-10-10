//! The Memory view, the recall line and the first-run import offer: reducer and screens.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{
    Event, ImportReport, MemoryChange, MemoryHit, MemoryKind, MemoryScope, MemoryStatsReport, MemoryTrust, Request,
};
use marlowe_tui::memory::MemMode;
use marlowe_tui::{Entry, State, View};

fn hit(id: &str, text: &str, kind: MemoryKind, trust: MemoryTrust, prov: &str) -> MemoryHit {
    MemoryHit {
        id: id.into(),
        text: text.into(),
        kind,
        trust,
        scope: MemoryScope::Global,
        occurred_at_ms: 1_790_000_000_000,
        provenance: prov.into(),
        score: 0.5,
    }
}

fn hits() -> Vec<MemoryHit> {
    let mut project = hit(
        "m3",
        "The gate threshold sweep ended at 0.41; anything higher dropped R@5.",
        MemoryKind::Result,
        MemoryTrust::Observed,
        "eval run 14",
    );
    project.scope = MemoryScope::Project("C:/work/valiance".into());
    vec![
        hit("m1", "Matthew prefers terse commit messages and no emoji.", MemoryKind::Preference, MemoryTrust::User, "you, session 3f2a"),
        hit("m2", "Chose cosine over dot product for the cue because vectors are unit length.", MemoryKind::Decision, MemoryTrust::Inferred, "imported from Marlowe_Harness"),
        project,
        hit("m4", "A web page claimed the model card was updated.", MemoryKind::Note, MemoryTrust::Untrusted, "web fetch"),
    ]
}

fn online() -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    s
}

fn in_memory() -> State {
    let mut s = online();
    assert_eq!(s.on_key(ctrl('g')), vec![Request::MemoryStats]);
    assert_eq!(s.view, View::Memory);
    feed(
        &mut s,
        0,
        Event::MemoryStats(MemoryStatsReport {
            beliefs: 1_204,
            episodes: 37,
            vectors: 1_198,
            models_loaded: vec![],
            degraded: None,
            journal_seq: 5_000,
        }),
    );
    type_str(&mut s, "gate");
    assert_eq!(
        s.on_key(key(KeyCode::Enter)),
        vec![Request::MemorySearch { query: "gate".into(), scope: None, limit: 30 }]
    );
    feed(&mut s, 0, Event::MemoryResults { hits: hits() });
    s
}

#[test]
fn memory_view_shows_results_with_kind_trust_date_provenance() {
    let s = in_memory();
    let buf = render(&s, W, H);
    for want in [
        "Memory",
        "1,204 memories",
        "preference \u{b7} you \u{b7} 2026-09-21",
        "inferred",
        "untrusted",
        "imported from Marlowe_Harness",
        "valiance",
        "search models unloaded",
    ] {
        assert!(contains(&buf, want), "missing {want:?}\n{}", dump(&buf));
    }
    assert_palette_only(&buf);
    snapshot("memory_results", &buf);
}

#[test]
fn memory_view_empty_and_degraded() {
    let mut s = online();
    s.on_key(ctrl('g'));
    feed(
        &mut s,
        0,
        Event::MemoryStats(MemoryStatsReport {
            degraded: Some("embedding model not found".into()),
            ..MemoryStatsReport::default()
        }),
    );
    let buf = render(&s, W, H);
    assert!(contains(&buf, "type a question or a name"));
    assert!(contains(&buf, "degraded: embedding model not found"));
    assert!(contains(&buf, "nothing is injected on its own"));
    assert_palette_only(&buf);
    snapshot("memory_degraded", &buf);

    type_str(&mut s, "zzz");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 0, Event::MemoryResults { hits: vec![] });
    assert!(contains(&render(&s, W, H), "nothing found"));
}

#[test]
fn selection_moves_and_stops_at_the_ends() {
    let mut s = in_memory();
    s.on_key(key(KeyCode::Down));
    s.on_key(key(KeyCode::Down));
    assert_eq!(s.memory.selected().unwrap().id, "m3");
    s.on_key(key(KeyCode::Down));
    s.on_key(key(KeyCode::Down));
    assert_eq!(s.memory.selected().unwrap().id, "m4");
    s.on_key(key(KeyCode::Up));
    assert_eq!(s.memory.selected().unwrap().id, "m3");
}

#[test]
fn forget_needs_confirmation() {
    let mut s = in_memory();
    s.on_key(key(KeyCode::Down));
    assert!(s.on_key(ctrl('d')).is_empty());
    assert!(matches!(&s.memory.mode, MemMode::ConfirmForget { id } if id == "m2"));
    let buf = render(&s, W, H);
    assert!(contains(&buf, "forget this memory?"));
    assert_palette_only(&buf);
    snapshot("memory_confirm_forget", &buf);

    // Anything but y/n/esc is ignored; n keeps it.
    assert!(s.on_key(key(KeyCode::Char('x'))).is_empty());
    assert!(matches!(s.memory.mode, MemMode::ConfirmForget { .. }));
    assert!(s.on_key(key(KeyCode::Char('n'))).is_empty());
    assert!(matches!(s.memory.mode, MemMode::Browse));
    assert_eq!(s.memory.hits.len(), 4);

    s.on_key(ctrl('d'));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::MemoryForget { id: "m2".into() }]);
    // The hit stays until the daemon confirms.
    assert_eq!(s.memory.hits.len(), 4);
    feed(&mut s, 5, Event::MemoryChanged { id: "m2".into(), change: MemoryChange::Forgotten });
    assert_eq!(s.memory.hits.len(), 3);
    assert!(!s.memory.hits.iter().any(|h| h.id == "m2"));
    assert!(contains(&render(&s, W, H), "forgotten"));
}

#[test]
fn correct_edits_in_place_and_sends_the_new_text() {
    let mut s = in_memory();
    assert!(s.on_key(ctrl('e')).is_empty());
    match &s.memory.mode {
        MemMode::Correct { id, input } => {
            assert_eq!(id, "m1");
            assert_eq!(input.text(), "Matthew prefers terse commit messages and no emoji.");
        }
        other => panic!("{other:?}"),
    }
    for _ in 0..6 {
        s.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut s, "or emoji.");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "correct this memory"));
    assert!(contains(&buf, "no or emoji."));
    assert_palette_only(&buf);
    snapshot("memory_correct", &buf);

    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(
        reqs,
        vec![Request::MemoryCorrect { id: "m1".into(), text: "Matthew prefers terse commit messages and no or emoji.".into() }]
    );
    assert!(matches!(s.memory.mode, MemMode::Browse));
    feed(&mut s, 6, Event::MemoryChanged { id: "m1".into(), change: MemoryChange::Corrected { by: "m9".into() } });
    assert_eq!(s.memory.hits.len(), 3);
}

#[test]
fn correct_escape_cancels_and_blank_is_refused() {
    let mut s = in_memory();
    s.on_key(ctrl('e'));
    s.on_key(ctrl('u'));
    assert!(s.on_key(key(KeyCode::Enter)).is_empty(), "a blank correction is not sent");
    assert!(matches!(s.memory.mode, MemMode::Correct { .. }));
    assert!(s.on_key(key(KeyCode::Esc)).is_empty());
    assert!(matches!(s.memory.mode, MemMode::Browse));
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.view, View::Conversation);
}

#[test]
fn empty_query_does_not_search() {
    let mut s = online();
    s.on_key(ctrl('g'));
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
}

#[test]
fn palette_has_memory_and_import_entries() {
    let mut s = online();
    s.on_key(ctrl('k'));
    type_str(&mut s, "memor");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Memory \u{2014} search, correct, forget"));
    assert!(contains(&buf, "Import memories from the old harness"));
    assert_palette_only(&buf);
    snapshot("palette_memory", &buf);

    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::MemoryStats]);
    assert_eq!(s.view, View::Memory);

    s.on_key(ctrl('k'));
    type_str(&mut s, "import");
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::ImportLegacy { path: None }]);
    assert!(s.import.is_some());
    assert!(contains(&render(&s, W, H), "importing the old memories"));
}

fn recalled_turn() -> State {
    let mut s = online();
    type_str(&mut s, "what threshold did we settle on?");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: "threshold".into(), workspace: None });
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "threshold".into(), workspace: None });
    feed(
        &mut s,
        3,
        Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "what threshold did we settle on?".into() },
    );
    s
}

#[test]
fn recall_line_appears_dim_before_the_answer() {
    let mut s = recalled_turn();
    feed(&mut s, 4, Event::Recalled { session: sid(), turn: 1, count: 3, summary: "gate sweep, cue choice".into() });
    assert!(matches!(s.entries.last(), Some(Entry::Recall { count: 3, .. })));
    // Still waiting for the model: the thinking line is not hidden by the recall line.
    let buf = render(&s, W, H);
    assert!(contains(&buf, "\u{22ef} recall \u{b7} 3 memories \u{b7} gate sweep, cue choice"));
    assert!(contains(&buf, "thinking\u{2026}"));
    feed(&mut s, 5, Event::TextDelta { session: sid(), turn: 1, text: "We settled on 0.41.".into() });
    let buf = render(&s, W, H);
    assert!(contains(&buf, "We settled on 0.41."));
    assert_palette_only(&buf);
    snapshot("recall_line", &buf);
}

#[test]
fn recall_of_nothing_or_another_session_shows_nothing() {
    let mut s = recalled_turn();
    feed(&mut s, 4, Event::Recalled { session: sid(), turn: 1, count: 0, summary: String::new() });
    feed(&mut s, 5, Event::Recalled { session: "other".into(), turn: 1, count: 2, summary: String::new() });
    assert!(!s.entries.iter().any(|e| matches!(e, Entry::Recall { .. })));
    feed(&mut s, 6, Event::Recalled { session: sid(), turn: 1, count: 1, summary: String::new() });
    assert!(contains(&render(&s, W, H), "recall \u{b7} 1 memory"));
}

fn offered() -> State {
    let mut s = online();
    feed(
        &mut s,
        2,
        Event::LegacyAvailable { path: "C:/old/default-profile".into(), approx_entries: 1_840, already_imported: false },
    );
    s
}

#[test]
fn first_run_offer_yes_imports() {
    let mut s = offered();
    let buf = render(&s, W, H);
    assert!(contains(&buf, "I found memories from the old harness"));
    assert!(contains(&buf, "about 1,840"));
    assert!(contains(&buf, "Not now"));
    assert_palette_only(&buf);
    snapshot("import_offer", &buf);

    assert_eq!(
        s.on_key(key(KeyCode::Enter)),
        vec![Request::ImportLegacy { path: Some("C:/old/default-profile".into()) }]
    );
    assert!(s.overlay.is_none());
    feed(&mut s, 3, Event::ImportProgress { read: 900, imported: 700 });
    s.on_key(ctrl('g'));
    assert!(contains(&render(&s, W, H), "900 read \u{b7} 700 imported"));
    feed(
        &mut s,
        4,
        Event::ImportDone {
            report: ImportReport { read: 1_840, imported: 1_700, skipped_duplicates: 140, ..ImportReport::default() },
        },
    );
    assert!(s.import.is_none() && s.legacy.is_none());
    let buf = render(&s, W, H);
    assert!(contains(&buf, "imported 1700 of 1840 memories, 140 already there"), "{}", dump(&buf));
    assert_palette_only(&buf);
    snapshot("import_done", &buf);
}

#[test]
fn first_run_offer_not_now_is_not_repeated() {
    let mut s = offered();
    s.on_key(key(KeyCode::Right));
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    assert!(s.overlay.is_none());
    // A reconnect re-sends the offer; the user already said not now.
    feed(
        &mut s,
        5,
        Event::LegacyAvailable { path: "C:/old/default-profile".into(), approx_entries: 1_840, already_imported: false },
    );
    assert!(s.overlay.is_none());
    // Esc is the same as Not now; n too.
    let mut s = offered();
    s.on_key(key(KeyCode::Esc));
    assert!(s.legacy_dismissed && s.overlay.is_none());
    let mut s = offered();
    assert!(s.on_key(key(KeyCode::Char('n'))).is_empty());
    assert!(s.legacy_dismissed);
    // The palette still offers it, with the path the daemon found.
    s.on_key(ctrl('k'));
    type_str(&mut s, "import");
    assert_eq!(
        s.on_key(key(KeyCode::Enter)),
        vec![Request::ImportLegacy { path: Some("C:/old/default-profile".into()) }]
    );
}

#[test]
fn already_imported_never_offers() {
    let mut s = online();
    feed(&mut s, 2, Event::LegacyAvailable { path: "p".into(), approx_entries: 9, already_imported: true });
    assert!(s.overlay.is_none() && s.legacy.is_none());
}

#[test]
fn an_error_ends_the_import_indicator() {
    let mut s = offered();
    s.on_key(key(KeyCode::Char('y')));
    assert!(s.import.is_some());
    feed(&mut s, 0, Event::Error { session: None, message: "the old profile is locked".into() });
    assert!(s.import.is_none());
}

#[test]
fn approval_takes_the_keys_over_the_offer() {
    let mut s = offered();
    feed(
        &mut s,
        3,
        Event::ApprovalRequested {
            approval: marlowe_proto::ApprovalId(1),
            session: sid(),
            tool: "bash".into(),
            summary: "run".into(),
            detail: "ls".into(),
            run: None,
            proposed_rule: None,
        },
    );
    s.on_key(key(KeyCode::Char('y')));
    assert!(s.import.is_none());
}

fn profile_hits() -> Vec<MemoryHit> {
    let mut decision = hit(
        "p2",
        "ship M3 before voice (reason: the Wake needs the brief first)",
        MemoryKind::Decision,
        MemoryTrust::User,
        "you",
    );
    decision.scope = MemoryScope::Project("C:/work/valiance".into());
    vec![
        hit("p1", "use uv, not pip", MemoryKind::Preference, MemoryTrust::Inferred, "conductor, session 3f2a1b9c turn 2"),
        decision,
        hit("p3", "the training cluster has eight A100s", MemoryKind::Fact, MemoryTrust::User, "you"),
    ]
}

#[test]
fn the_profile_filter_lists_what_is_in_context_now() {
    let mut s = in_memory();
    // ^p asks for the profile of the shown workspace and switches the list.
    assert_eq!(s.on_key(ctrl('p')), vec![Request::MemoryProfile { workspace: Some("C:/work/valiance".into()) }]);
    assert!(s.memory.shown().is_empty(), "nothing until the daemon answers");
    assert!(contains(&render(&s, W, H), "reading the profile"));
    feed(&mut s, 0, Event::MemoryProfile { hits: profile_hits(), tokens: 64, omitted: 2 });
    assert_eq!(s.memory.selected().unwrap().id, "p1");
    let buf = render(&s, W, H);
    for want in [
        "profile",
        "in context every turn \u{b7} 3 items \u{b7} ~64 tokens",
        "2 left out by the budget",
        "use uv, not pip",
        "preference \u{b7} inferred",
        "ship M3 before voice",
        "^p results",
    ] {
        assert!(contains(&buf, want), "missing {want:?}\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "The gate threshold sweep"), "search results are hidden");
    assert_palette_only(&buf);
    snapshot("memory_profile", &buf);

    // Forget and correct act on the profile item; a change re-reads the profile.
    s.on_key(key(KeyCode::Down));
    s.on_key(ctrl('d'));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::MemoryForget { id: "p2".into() }]);
    let reqs = feed(&mut s, 7, Event::MemoryChanged { id: "p2".into(), change: MemoryChange::Forgotten });
    assert_eq!(reqs, vec![Request::MemoryProfile { workspace: Some("C:/work/valiance".into()) }]);
    assert_eq!(s.memory.shown().len(), 2);
    assert_eq!(s.memory.hits.len(), 4, "search results are untouched");

    // ^p again returns to the search results without a request; a search also leaves the profile.
    assert!(s.on_key(ctrl('p')).is_empty());
    assert_eq!(s.memory.shown().len(), 4);
    s.on_key(ctrl('p'));
    type_str(&mut s, "x");
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::MemorySearch { query: "gatex".into(), scope: None, limit: 30 }]);
    assert_eq!(s.memory.shown().len(), 4);
}

#[test]
fn an_empty_profile_says_so() {
    let mut s = in_memory();
    s.on_key(ctrl('p'));
    feed(&mut s, 0, Event::MemoryProfile { hits: vec![], tokens: 0, omitted: 0 });
    let buf = render(&s, W, H);
    assert!(contains(&buf, "0 items") && contains(&buf, "nothing recorded yet"), "{}", dump(&buf));
    assert_palette_only(&buf);
    // A change elsewhere in memory while the profile is shown re-reads it.
    assert_eq!(
        feed(&mut s, 8, Event::MemoryChanged { id: "m9".into(), change: MemoryChange::Remembered }),
        vec![Request::MemoryProfile { workspace: Some("C:/work/valiance".into()) }]
    );
}
