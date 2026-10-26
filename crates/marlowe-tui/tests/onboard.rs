//! First-run onboarding: name, address form, the checklist; when it shows and when it stays away.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{Event, MemoryStatsReport, ModelsState, Request, WatchCandidate};
use marlowe_tui::onboard::{ItemKind, ItemState, Onboard, Step, MODELS_PURPOSE, ONBOARD_GUARD_MS};
use marlowe_tui::state::Overlay;
use marlowe_tui::wake::BOOT_FULL_MS;
use marlowe_tui::{State, View};

fn unset() -> Event {
    Event::User { name: None, address: None, set: false }
}

fn legacy() -> Event {
    Event::LegacyAvailable { path: "C:/old/default-profile".into(), approx_entries: 1_840, already_imported: false }
}

fn folder(path: &str, reason: &str) -> WatchCandidate {
    WatchCandidate { path: path.into(), reason: reason.into(), modified_ms: 1, live_runs: 0 }
}

fn folders() -> Vec<WatchCandidate> {
    vec![
        folder("D:/lab/PruningResearch/valiance", "heartbeat.json, 3 logs"),
        folder("D:/lab/llama-sweeps", "2 metric files"),
    ]
}

/// A first launch as the daemon sends it: Welcome, User (unset), the legacy offer, an empty
/// library. The boot is running.
fn first_launch() -> State {
    let mut s = state();
    s.start_boot(None);
    welcome(&mut s, 0);
    feed(&mut s, 0, unset());
    feed(&mut s, 0, legacy());
    feed(&mut s, 1, empty_library());
    s
}

/// The boot is over and the guard has passed.
fn after_boot(s: &mut State) -> Vec<Request> {
    s.now_ms += BOOT_FULL_MS + 1;
    let out = s.on_tick();
    s.now_ms += ONBOARD_GUARD_MS + 1;
    out
}

fn step(s: &State) -> Step {
    match &s.onboard {
        Onboard::Active { step, .. } => step.clone(),
        other => panic!("onboarding is not on screen: {other:?}"),
    }
}

fn shot(s: &State, name: &str) {
    let buf = render(s, W, H);
    assert_palette_only(&buf);
    snapshot(name, &buf);
}

#[test]
fn it_waits_for_the_boot_and_holds_the_import_offer() {
    let mut s = first_launch();
    assert_eq!(s.onboard, Onboard::Pending);
    assert!(s.overlay.is_none(), "the legacy offer waits for onboarding");
    assert!(s.on_tick().is_empty(), "nothing while booting");
    assert!(s.overlay.is_none());
    // Discovery is asked for as it opens, for the checklist.
    assert_eq!(after_boot(&mut s), vec![Request::WatchCandidates]);
    assert_eq!(s.overlay, Some(Overlay::Onboard));
    assert!(matches!(step(&s), Step::Name { .. }));
}

#[test]
fn name_address_and_checklist_screens() {
    let mut s = first_launch();
    after_boot(&mut s);
    type_str(&mut s, "Matthew");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "What should I call you?"));
    assert!(contains(&buf, "› Matthew"));
    shot(&s, "onboard_name");

    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    let buf = render(&s, W, H);
    assert!(contains(&buf, "In greetings: \u{201c}sir\u{201d}, your name, or nothing?"));
    assert!(contains(&buf, "› 1 sir"), "sir is the default");
    assert!(contains(&buf, "2 Matthew") && contains(&buf, "3 nothing"));
    shot(&s, "onboard_address");

    // Discovery answers while the name is typed.
    feed(&mut s, 2, Event::WatchCandidates { candidates: folders() });
    assert_eq!(
        s.on_key(key(KeyCode::Enter)),
        vec![Request::SetUser { name: Some("Matthew".into()), address: Some("sir".into()) }]
    );
    let Step::Checklist { items, sel } = step(&s) else { panic!("{:?}", s.onboard) };
    assert_eq!(sel, 0);
    let kinds: Vec<&str> = items
        .iter()
        .map(|i| match &i.kind {
            ItemKind::Import { .. } => "import",
            ItemKind::Watch(_) => "watch",
            ItemKind::AddModel => "model",
        })
        .collect();
    assert_eq!(kinds, ["import", "watch", "watch", "model"]);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Import memories from the old harness · about 1,840"));
    assert!(contains(&buf, "Watch D:/lab/PruningResearch/valiance · heartbeat.json, 3 logs"), "{}", dump(&buf));
    assert!(contains(&buf, "Add a model · opens the library"));
    assert!(contains(&buf, "saved · Matthew · \u{201c}sir\u{201d} in greetings"), "the choice is confirmed under the composer");
    shot(&s, "onboard_checklist");

    // Each item is its own consent: import yes, first folder skipped, second folder yes.
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::ImportLegacy { path: Some("C:/old/default-profile".into()) }]);
    assert!(s.on_key(key(KeyCode::Char('s'))).is_empty());
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::WatchAdd { path: "D:/lab/llama-sweeps".into() }]);
    let Step::Checklist { items, sel } = step(&s) else { panic!() };
    let states: Vec<ItemState> = items.iter().map(|i| i.state).collect();
    assert_eq!(states, [ItemState::Done, ItemState::Skipped, ItemState::Done, ItemState::Open]);
    assert_eq!(sel, 3);
    shot(&s, "onboard_checklist_progress");

    // Add a model leaves for the library and ends onboarding.
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    assert_eq!(s.view, View::Library);
    assert!(s.overlay.is_none());
    assert_eq!(s.onboard, Onboard::Done);
}

#[test]
fn esc_skips_everything_and_it_never_comes_back_this_session() {
    let mut s = first_launch();
    after_boot(&mut s);
    type_str(&mut s, "Matt");
    assert!(s.on_key(key(KeyCode::Esc)).is_empty(), "nothing is saved");
    assert!(s.overlay.is_none());
    assert_eq!(s.onboard, Onboard::Done);
    // A reconnect repeats the unset user and the legacy offer; neither nags.
    welcome(&mut s, 1);
    feed(&mut s, 0, unset());
    assert!(s.on_tick().is_empty());
    assert!(s.overlay.is_none());
    // The palette brings it back on request, with no type-ahead guard.
    s.on_key(ctrl('k'));
    type_str(&mut s, "your name");
    s.on_key(key(KeyCode::Enter));
    assert_eq!(s.overlay, Some(Overlay::Onboard));
    type_str(&mut s, "M");
    assert!(matches!(step(&s), Step::Name { input } if input.text() == "M"));
}

#[test]
fn typing_during_the_boot_defers_it_and_keeps_every_key() {
    let mut s = first_launch();
    type_str(&mut s, "hi");
    assert_eq!(s.composer.text(), "hi");
    assert_eq!(s.onboard, Onboard::Done);
    s.now_ms += BOOT_FULL_MS + 1;
    assert!(s.on_tick().is_empty());
    assert!(s.overlay.is_none());
}

#[test]
fn a_fast_typist_is_not_caught_by_the_overlay() {
    let mut s = first_launch();
    s.now_ms += BOOT_FULL_MS + 1;
    s.on_tick();
    assert_eq!(s.overlay, Some(Overlay::Onboard));
    // Within the guard: the keys were meant for the composer.
    type_str(&mut s, "ls");
    assert!(s.overlay.is_none());
    assert_eq!(s.onboard, Onboard::Done);
    assert_eq!(s.composer.text(), "ls");
}

#[test]
fn a_non_typing_key_during_the_boot_only_ends_it() {
    let mut s = first_launch();
    s.on_key(key(KeyCode::Esc));
    assert!(!s.booting());
    assert_eq!(s.onboard, Onboard::Pending);
    assert_eq!(s.on_tick(), vec![Request::WatchCandidates]);
    assert_eq!(s.overlay, Some(Overlay::Onboard));
}

#[test]
fn a_known_user_is_greeted_and_never_onboarded() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 0, Event::User { name: Some("Matthew".into()), address: Some("sir".into()), set: true });
    feed(&mut s, 1, library_with_sonnet());
    assert!(s.on_tick().is_empty());
    assert_eq!(s.onboard, Onboard::Idle);
    assert!(contains(&render(&s, W, H), "Good evening, sir."));
    // Set elsewhere while pending: it does not show.
    let mut s = first_launch();
    feed(&mut s, 2, Event::User { name: Some("Matthew".into()), address: None, set: true });
    assert_eq!(s.onboard, Onboard::Done);
    s.now_ms += BOOT_FULL_MS + 1;
    assert!(s.on_tick().is_empty() && s.overlay.is_none());
    assert!(contains(&render(&s, W, H), "You have no models."));
}

#[test]
fn address_choices_follow_the_name() {
    // No name: sir or nothing.
    let mut s = first_launch();
    after_boot(&mut s);
    s.on_key(key(KeyCode::Enter));
    let buf = render(&s, W, H);
    assert!(contains(&buf, "1 sir") && contains(&buf, "2 nothing") && !contains(&buf, "3 "));
    assert_eq!(s.on_key(key(KeyCode::Char('2')))[0], Request::SetUser { name: None, address: None });

    // A name: the second choice is the name itself; backspace returns to the name.
    let mut s = first_launch();
    after_boot(&mut s);
    type_str(&mut s, "Matthew");
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Backspace));
    assert!(matches!(step(&s), Step::Name { input } if input.text() == "Matthew"));
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Right));
    assert_eq!(
        s.on_key(key(KeyCode::Enter))[0],
        Request::SetUser { name: Some("Matthew".into()), address: Some("Matthew".into()) }
    );
}

#[test]
fn only_relevant_steps_are_listed_and_nothing_relevant_finishes() {
    // Models present, no old profile, discovery found nothing: done after the address.
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 0, unset());
    feed(&mut s, 1, library_with_sonnet());
    assert_eq!(s.on_tick(), vec![Request::WatchCandidates]);
    s.now_ms += ONBOARD_GUARD_MS + 1;
    feed(&mut s, 0, Event::WatchCandidates { candidates: vec![] });
    s.on_key(key(KeyCode::Enter));
    assert_eq!(s.on_key(key(KeyCode::Enter)).len(), 1);
    assert!(s.overlay.is_none());
    assert_eq!(s.onboard, Onboard::Done);
    assert!(contains(&render(&s, W, H), "Good evening, sir."), "the new address is used at once");

    // Discovery still out when the checklist opens: folders join it when they arrive.
    let mut s = first_launch();
    after_boot(&mut s);
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Enter));
    assert!(contains(&render(&s, W, H), "looking for run folders"));
    feed(&mut s, 2, Event::WatchCandidates { candidates: folders() });
    let Step::Checklist { items, .. } = step(&s) else { panic!() };
    assert!(matches!(items[1].kind, ItemKind::Watch(_)) && items[3].kind == ItemKind::AddModel);
    // Esc in the checklist is done.
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.onboard, Onboard::Done);
    assert!(s.overlay.is_none());
}

#[test]
fn approvals_take_the_keys_over_onboarding() {
    let mut s = first_launch();
    after_boot(&mut s);
    feed(
        &mut s,
        2,
        Event::ApprovalRequested {
            approval: marlowe_proto::ApprovalId(1),
            session: sid(),
            tool: "bash".into(),
            summary: "cargo test".into(),
            detail: "{}".into(),
            run: None,
            proposed_rule: None,
        },
    );
    type_str(&mut s, "x");
    assert!(matches!(step(&s), Step::Name { input } if input.text().is_empty()));
}

#[test]
fn a_skipped_import_is_not_offered_again_on_a_reconnect() {
    let mut s = first_launch();
    after_boot(&mut s);
    s.on_key(key(KeyCode::Esc));
    welcome(&mut s, 1);
    feed(&mut s, 0, unset());
    feed(&mut s, 0, legacy());
    assert!(s.overlay.is_none(), "{:?}", s.overlay);
    // Deferred by typing: the same.
    let mut s = first_launch();
    type_str(&mut s, "x");
    welcome(&mut s, 1);
    feed(&mut s, 0, legacy());
    assert!(s.overlay.is_none());
    assert!(s.legacy.is_some(), "the palette still has it");
}

#[test]
fn a_checklist_waiting_on_a_missing_sentinel_closes_instead_of_hanging() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 0, unset());
    feed(&mut s, 1, library_with_sonnet());
    assert_eq!(s.on_tick(), vec![Request::WatchCandidates]);
    s.now_ms += ONBOARD_GUARD_MS + 1;
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Enter));
    assert!(matches!(step(&s), Step::Checklist { items, .. } if items.is_empty()), "waiting for discovery");
    feed(&mut s, 2, Event::Error { session: None, message: marlowe_tui::wake::NO_SENTINEL.into() });
    s.on_tick();
    assert_eq!(s.onboard, Onboard::Done);
    assert!(s.overlay.is_none());
}

// ---------------------------------------------------------------- the memory models

const SHIPPED: u64 = 193_270_110;

fn stats_with(models: ModelsState) -> Event {
    Event::MemoryStats(MemoryStatsReport {
        beliefs: 390,
        degraded: (models != ModelsState::Ready).then(|| "models missing".to_string()),
        models,
        ..Default::default()
    })
}

fn missing() -> Event {
    stats_with(ModelsState::Missing { bytes: SHIPPED, cascade: false })
}

fn memory_line(s: &State) -> String {
    s.systems().into_iter().find(|l| l.label == "memory").expect("a memory line").value
}

/// A first launch whose daemon says the memory models are not on this machine.
fn first_launch_without_models() -> State {
    let mut s = first_launch();
    feed(&mut s, 2, missing());
    s
}

/// Through the name and the address to the next step.
fn name_and_address(s: &mut State) {
    after_boot(s);
    type_str(s, "Matthew");
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Enter));
}

#[test]
fn the_memory_models_are_offered_after_the_name_with_their_purpose_and_size() {
    let mut s = first_launch_without_models();
    name_and_address(&mut s);
    assert!(matches!(step(&s), Step::Models { bytes: SHIPPED, cascade: false, sel: 0 }), "{:?}", s.onboard);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Install memory LMs? \u{b7} 193 MB"), "{}", dump(&buf));
    // One sentence on what they are for, wrapped to the panel.
    assert!(contains(&buf, "recall our past conversations by meaning, not just"), "{}", dump(&buf));
    assert!(contains(&buf, "matching words. Without them, memory is keyword-only."), "{}", dump(&buf));
    assert!(MODELS_PURPOSE.contains("keyword-only"));
    assert!(contains(&buf, "\u{203a} 1 yes") && contains(&buf, "2 no"));
    assert!(!contains(&buf, "GPU rerank cascade"), "the cascade is only on a cuda build");
    shot(&s, "onboard_models");
}

#[test]
fn a_cuda_build_shows_the_larger_size_in_the_same_question() {
    let mut s = first_launch();
    feed(&mut s, 2, stats_with(ModelsState::Missing { bytes: 651_769_316, cascade: true }));
    name_and_address(&mut s);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Install memory LMs? \u{b7} 652 MB"), "{}", dump(&buf));
    assert!(contains(&buf, "Includes the GPU rerank cascade."));
}

#[test]
fn the_question_is_not_asked_when_the_models_are_here_or_already_answered() {
    for models in [ModelsState::Ready, ModelsState::Declined, ModelsState::Installing { done: 1, total: 2 }] {
        let mut s = first_launch();
        feed(&mut s, 2, stats_with(models.clone()));
        name_and_address(&mut s);
        assert!(matches!(step(&s), Step::Checklist { .. }), "{models:?}: {:?}", s.onboard);
        assert!(!contains(&render(&s, W, H), "Install memory LMs?"));
    }
    // Stats that have not arrived are not a reason to ask either.
    let mut s = first_launch();
    name_and_address(&mut s);
    assert!(matches!(step(&s), Step::Checklist { .. }));
}

#[test]
fn yes_sends_the_install_request_and_the_memory_line_follows_the_daemon() {
    let mut s = first_launch_without_models();
    name_and_address(&mut s);
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::ModelsInstall], "yes is the default");
    assert!(matches!(step(&s), Step::Checklist { .. }), "on to the other steps");
    assert!(memory_line(&s).contains("installing memory LMs 0% (0/193 MB)"), "{}", memory_line(&s));
    // The daemon's progress replaces the report; the line reads the figures.
    feed(&mut s, 0, stats_with(ModelsState::Installing { done: 78_000_000, total: 186_000_000 }));
    assert_eq!(memory_line(&s), "390 beliefs \u{b7} installing memory LMs 41% (78/186 MB)");
    feed(&mut s, 3, Event::MemoryStats(MemoryStatsReport { beliefs: 390, ..Default::default() }));
    assert_eq!(memory_line(&s), "390 beliefs \u{b7} recall online", "loaded without a restart");
}

#[test]
fn no_is_remembered_and_the_memory_line_says_how_to_install_later() {
    let mut s = first_launch_without_models();
    name_and_address(&mut s);
    assert_eq!(s.on_key(key(KeyCode::Char('n'))), vec![Request::ModelsDecline]);
    assert!(memory_line(&s).contains("marlowe models fetch"), "{}", memory_line(&s));
    assert!(memory_line(&s).contains("keyword only"));
    // The daemon remembers it; a later launch gets Declined and no question.
    let mut again = first_launch();
    feed(&mut again, 2, stats_with(ModelsState::Declined));
    name_and_address(&mut again);
    assert!(matches!(step(&again), Step::Checklist { .. }));
    assert!(memory_line(&again).contains("marlowe models fetch"));
    // Moving to "no" and pressing Enter is the same answer.
    let mut s = first_launch_without_models();
    name_and_address(&mut s);
    s.on_key(key(KeyCode::Right));
    assert!(matches!(step(&s), Step::Models { sel: 1, .. }));
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::ModelsDecline]);
}

#[test]
fn a_failed_download_is_shown_with_where_it_is_not_published() {
    let mut s = first_launch();
    let url = "https://huggingface.co/devvexus/marlowe-memory/resolve/main/ms-marco-MiniLM-L-2-v2-ft-session-j/model.onnx";
    feed(&mut s, 2, stats_with(ModelsState::Failed { reason: format!("not published at {url}") }));
    assert_eq!(memory_line(&s), "390 beliefs \u{b7} memory LMs not published at huggingface.co/devvexus/marlowe-memory: keyword only \u{b7} marlowe models fetch retries");
    // It is not asked again for: the failure is not a prompt.
    name_and_address(&mut s);
    assert!(matches!(step(&s), Step::Checklist { .. }));
}

#[test]
fn a_user_who_is_already_named_is_still_offered_the_models() {
    let named = || {
        let mut s = state();
        s.start_boot(None);
        welcome(&mut s, 0);
        feed(&mut s, 0, Event::User { name: Some("Matthew".into()), address: Some("sir".into()), set: true });
        feed(&mut s, 1, library_with_sonnet());
        s
    };
    let mut s = named();
    assert_eq!(s.onboard, Onboard::Idle);
    feed(&mut s, 2, missing());
    assert_eq!(s.onboard, Onboard::Pending);
    after_boot(&mut s);
    assert!(matches!(step(&s), Step::Models { .. }), "no name step for a named user: {:?}", s.onboard);
    assert_eq!(s.overlay, Some(Overlay::Onboard));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::ModelsInstall]);
    // Discovery found nothing, so nothing else applies.
    feed(&mut s, 3, Event::WatchCandidates { candidates: vec![] });
    assert_eq!(s.onboard, Onboard::Done);
    assert!(s.overlay.is_none());

    // Skipped with Esc, it does not come back this session.
    let mut s = named();
    feed(&mut s, 2, missing());
    after_boot(&mut s);
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.onboard, Onboard::Done);
    feed(&mut s, 2, missing());
    assert_eq!(s.onboard, Onboard::Done);
}
