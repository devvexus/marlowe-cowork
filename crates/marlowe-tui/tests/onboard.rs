//! First-run onboarding: name, address form, the checklist; when it shows and when it stays away.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{Event, Request, WatchCandidate};
use marlowe_tui::onboard::{ItemKind, ItemState, Onboard, Step, ONBOARD_GUARD_MS};
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
