//! The engine window, the title bar's `engine: <backend>`, the release prompt and the first-run
//! engine offer: what each state shows, what the keys and clicks send, and that nothing is a fill.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{BackendBuild, BackendOp, BackendReport, BackendUpdate, Event, RemoteTag, Request};
use marlowe_tui::engines::UPGRADE_GUARD_MS;
use marlowe_tui::onboard::{Onboard, Step, ONBOARD_GUARD_MS};
use marlowe_tui::state::Overlay;
use marlowe_tui::wake::BOOT_FULL_MS;
use marlowe_tui::State;
use ratatui::layout::Rect;

/// Three hours before the fixed test clock.
const CHECKED: u64 = 1_800_000_000 - 3 * 3600;

fn llama() -> BackendReport {
    BackendReport {
        id: "llama.cpp".into(),
        title: "llama.cpp".into(),
        description: "The standard llama.cpp backend.".into(),
        repo: "ggml-org/llama.cpp".into(),
        approx_bytes: 710_000_000,
        selected: true,
        installed: Some(BackendBuild {
            tag: "v1.0.0".into(),
            label: "v1.0.0".into(),
            release: Some("v1.0.0".into()),
            build: Some("b12359".into()),
            date: Some("2026-11-02T10:11:12Z".into()),
            device: "gpu".into(),
            note: None,
        }),
        latest_release: Some(RemoteTag { tag: "v1.0.0".into(), date: None }),
        newest_build: Some(RemoteTag { tag: "b12556".into(), date: None }),
        state: BackendUpdate::UpToDate,
        builds_newer: Some(197),
        last_checked: Some(CHECKED),
        ..Default::default()
    }
}

fn ms() -> BackendReport {
    BackendReport {
        id: "ms-llama.cpp".into(),
        title: "ms-llama.cpp".into(),
        description: "A much more accurate MS-capable model backend.".into(),
        repo: "devvexus/ms-llama.cpp".into(),
        approx_bytes: 751_000_000,
        state: BackendUpdate::NotInstalled,
        ..Default::default()
    }
}

fn installed_ms() -> BackendReport {
    BackendReport {
        installed: Some(BackendBuild {
            tag: "ms-v1.0".into(),
            label: "ms-v1.0".into(),
            release: Some("ms-v1.0".into()),
            date: Some("2026-10-20T08:00:00Z".into()),
            device: "gpu".into(),
            ..Default::default()
        }),
        latest_release: Some(RemoteTag { tag: "ms-v1.0".into(), date: None }),
        state: BackendUpdate::UpToDate,
        builds_newer: Some(0),
        last_checked: Some(CHECKED),
        ..ms()
    }
}

fn backends(selected: &str, list: Vec<BackendReport>) -> Event {
    Event::Backends { selected: selected.into(), backends: list }
}

/// A conversation with the daemon online and the backends known.
fn with(selected: &str, list: Vec<BackendReport>) -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 2, backends(selected, list));
    s
}

fn up_to_date() -> State {
    with("llama.cpp", vec![llama(), ms()])
}

fn open(s: &mut State) -> Vec<Request> {
    s.on_key(ctrl('k'));
    type_str(s, "engine");
    s.on_key(key(KeyCode::Enter))
}

fn shot(s: &State, name: &str) {
    let buf = render(s, W, H);
    assert_palette_only(&buf);
    snapshot(name, &buf);
}

fn find(buf: &ratatui::buffer::Buffer, needle: &str) -> (u16, u16) {
    for (y, line) in dump(buf).lines().enumerate() {
        if let Some(i) = line.find(needle) {
            return (line[..i].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle} not on screen:\n{}", dump(buf));
}

fn click(s: &mut State, w: u16, h: u16, at: (u16, u16)) -> Vec<Request> {
    let area = Rect::new(0, 0, w, h);
    let m = |kind| MouseEvent { kind, column: at.0, row: at.1, modifiers: KeyModifiers::NONE };
    s.on_mouse(m(MouseEventKind::Moved), area);
    s.on_mouse(m(MouseEventKind::Down(MouseButton::Left)), area)
}

// ---------------------------------------------------------------- the title bar

#[test]
fn the_title_bar_names_the_backend_and_a_click_opens_the_window() {
    let mut s = up_to_date();
    let buf = render(&s, W, H);
    assert_palette_only(&buf);
    let header = dump(&buf).lines().next().unwrap().to_string();
    assert!(header.contains("engine: llama.cpp"), "{header}");
    assert!(!header.contains("not installed"));
    snapshot("engine_title_bar", &buf);

    let at = find(&buf, "engine: llama.cpp");
    let out = click(&mut s, W, H, at);
    assert_eq!(out, vec![Request::Backends]);
    assert!(matches!(s.overlay, Some(Overlay::Engines { sel: 0, button: 0 })), "{:?}", s.overlay);

    // The other backend, and an install that is still missing, are said in the bar.
    let s = with("ms-llama.cpp", vec![llama(), installed_ms()]);
    assert!(dump(&render(&s, W, H)).lines().next().unwrap().contains("engine: ms-llama.cpp"));
    let s = with("llama.cpp", vec![BackendReport { state: BackendUpdate::NotInstalled, installed: None, ..llama() }, ms()]);
    let buf = render(&s, W, H);
    assert_palette_only(&buf);
    assert!(dump(&buf).lines().next().unwrap().contains("engine: llama.cpp \u{b7} not installed"));
    let s = with("llama.cpp", vec![BackendReport { state: BackendUpdate::UpdateAvailable, ..llama() }, ms()]);
    assert!(dump(&render(&s, W, H)).lines().next().unwrap().contains("engine: llama.cpp \u{2191}"));

    // Before the daemon has said, and offline, the bar says nothing about it.
    let mut s = state();
    welcome(&mut s, 0);
    assert!(!dump(&render(&s, W, H)).contains("engine:"));
}

#[test]
fn the_palette_opens_the_window_and_the_welcome_asks_for_the_backends() {
    let mut s = state();
    let reqs = welcome(&mut s, 0);
    assert!(reqs.contains(&Request::Backends));
    let mut s = up_to_date();
    assert_eq!(open(&mut s), vec![Request::Backends]);
    assert!(matches!(s.overlay, Some(Overlay::Engines { .. })));
    s.on_key(key(KeyCode::Esc));
    assert!(s.overlay.is_none());
}

// ---------------------------------------------------------------- the window

#[test]
fn the_window_for_an_installed_up_to_date_backend() {
    let mut s = up_to_date();
    open(&mut s);
    let buf = render(&s, W, H);
    let t = dump(&buf);
    assert!(t.contains("serving local models"), "{t}");
    assert!(t.contains("v1.0.0 \u{b7} build b12359 \u{b7} 2026-11-02 \u{b7} GPU"), "{t}");
    assert!(t.contains("up to date"));
    assert!(t.contains("197 newer builds upstream \u{b7} newest b12556 \u{b7} checked 3h ago"), "{t}");
    assert!(t.contains("A much more accurate MS-capable model backend."));
    assert!(t.contains("not installed") && t.contains("about 751 MB to download"));
    assert!(t.contains("[Check for updates]") && t.contains("[Sync to upstream]"));
    assert_eq!(t.matches("[Select]").count(), 1, "only the other backend can be selected");
    assert!(!t.contains("Upgrade"), "an up-to-date backend has nothing to upgrade to");
    shot(&s, "engine_window_up_to_date");
}

#[test]
fn the_window_for_an_update_a_sync_and_a_warning() {
    let mut ms_up = installed_ms();
    ms_up.state = BackendUpdate::UpdateAvailable;
    ms_up.latest_release = Some(RemoteTag { tag: "ms-v2.0".into(), date: None });
    ms_up.upgrade_warning = Some("ms-v2.0 stops running the Gen 1 MS models: Qwen3.8-27B-MS2.7.".into());
    let mut s = with("llama.cpp", vec![BackendReport { state: BackendUpdate::UpdateAvailable, latest_release: Some(RemoteTag { tag: "v1.1.0".into(), date: None }), ..llama() }, ms_up]);
    open(&mut s);
    let buf = render(&s, W, H);
    let t = dump(&buf);
    assert!(t.contains("update available: v1.1.0") && t.contains("[Upgrade to v1.1.0]"), "{t}");
    assert!(t.contains("update available: ms-v2.0") && t.contains("! ms-v2.0 stops running the Gen 1 MS models"), "{t}");
    shot(&s, "engine_window_update_available");

    // 120x30: everything still shows.
    let buf = render(&s, 120, 30);
    assert_palette_only(&buf);
    assert!(contains(&buf, "[Upgrade to v1.1.0]") && contains(&buf, "[Sync to upstream]") && contains(&buf, "Release prompts: on"));
    snapshot("engine_window_120x30", &buf);
}

#[test]
fn the_window_for_a_download_in_progress() {
    let newer = BackendReport { state: BackendUpdate::UpdateAvailable, latest_release: Some(RemoteTag { tag: "v1.1.0".into(), date: None }), ..llama() };
    let mut s = with("llama.cpp", vec![newer, ms()]);
    open(&mut s);
    // Pressing Upgrade shows it running at once, before the daemon says anything.
    s.on_key(key(KeyCode::Char('u')));
    assert!(contains(&render(&s, W, H), "starting"));
    feed(
        &mut s,
        0,
        Event::BackendProgress { backend: "llama.cpp".into(), op: BackendOp::Install, message: "downloading llama-b12359-bin-win-cuda-12.4-x64.zip".into(), done: 310_000_000, total: 710_000_000 },
    );
    let buf = render(&s, W, H);
    let t = dump(&buf);
    assert!(t.contains(" 44%") && t.contains("310 of 710 MB") && t.contains("downloading llama-b12359"), "{t}");
    // Every button is off while it runs: pressing one says why.
    s.on_key(key(KeyCode::Char('c')));
    assert!(s.flash.as_ref().unwrap().text.contains("busy"), "{:?}", s.flash);
    assert!(dump(&render(&s, W, H)).contains("\u{2501}"), "the bar is drawn");
    shot(&s, "engine_window_downloading");
    // The title bar says it too.
    assert!(dump(&render(&s, W, H)).lines().next().unwrap().contains("engine: llama.cpp \u{2026}"));

    feed(&mut s, 0, Event::BackendDone { backend: "llama.cpp".into(), op: BackendOp::Install, ok: true, message: "llama.cpp v1.1.0 is installed".into() });
    feed(&mut s, 3, backends("llama.cpp", vec![llama(), ms()]));
    assert!(!contains(&render(&s, W, H), "downloading"));
}

#[test]
fn the_window_for_a_backend_that_is_not_installed() {
    let mut s = with("llama.cpp", vec![BackendReport { state: BackendUpdate::NotInstalled, installed: None, builds_newer: None, last_checked: None, latest_release: None, newest_build: None, ..llama() }, ms()]);
    open(&mut s);
    let buf = render(&s, W, H);
    let t = dump(&buf);
    assert!(t.contains("[Install]") && t.contains("about 710 MB to download"), "{t}");
    assert!(t.contains("builds not looked at yet"));
    shot(&s, "engine_window_not_installed");
    // Selecting something that is not installed is refused with the reason, not sent.
    s.on_key(key(KeyCode::Down));
    let out = s.on_key(key(KeyCode::Char('s')));
    assert!(out.is_empty());
    assert!(s.flash.as_ref().unwrap().text.contains("install it first"), "{:?}", s.flash);
}

#[test]
fn the_window_shows_a_failure_and_a_cpu_fallback() {
    let mut s = up_to_date();
    open(&mut s);
    s.on_key(key(KeyCode::Char('c')));
    feed(&mut s, 0, Event::BackendDone { backend: "llama.cpp".into(), op: BackendOp::Check, ok: false, message: "GitHub rate limit: try again after 21:40".into() });
    let t = dump(&render(&s, W, H));
    assert!(t.contains("\u{2717} GitHub rate limit: try again after 21:40"), "{t}");
    shot(&s, "engine_window_error");

    let mut cpu = llama();
    if let Some(i) = cpu.installed.as_mut() {
        i.device = "cpu".into();
        i.note = Some("no CUDA device found".into());
    }
    feed(&mut s, 3, backends("llama.cpp", vec![cpu, ms()]));
    let t = dump(&render(&s, W, H));
    assert!(t.contains("CPU only") && t.contains("no GPU seen by the smoke test"), "{t}");
    // The next operation clears the old failure.
    s.on_key(key(KeyCode::Char('c')));
    assert!(s.engines.errors.is_empty());
    assert!(!dump(&render(&s, W, H)).contains("\u{2717}"));
}

// ---------------------------------------------------------------- keys and clicks

#[test]
fn the_keys_send_what_the_buttons_say() {
    let mut s = with("llama.cpp", vec![llama(), installed_ms()]);
    open(&mut s);

    // Select the other backend: down, then s. A short status line says what happens.
    s.on_key(key(KeyCode::Down));
    let out = s.on_key(key(KeyCode::Char('s')));
    assert_eq!(out, vec![Request::BackendSelect { backend: "ms-llama.cpp".into() }]);
    assert!(s.flash.as_ref().unwrap().text.contains("switching"), "{:?}", s.flash);
    feed(&mut s, 0, backends("ms-llama.cpp", vec![BackendReport { selected: false, ..llama() }, BackendReport { selected: true, ..installed_ms() }]));
    feed(&mut s, 0, Event::BackendDone { backend: "ms-llama.cpp".into(), op: BackendOp::Select, ok: true, message: "ms-llama.cpp serves models now".into() });
    assert_eq!(s.flash.as_ref().unwrap().text, "ms-llama.cpp serves models now");
    assert!(dump(&render(&s, W, H)).lines().next().unwrap().contains("engine: ms-llama.cpp"));

    // Check, then Sync (nothing is newer for the ms backend, so it says so instead).
    assert_eq!(s.on_key(key(KeyCode::Char('c'))), vec![Request::BackendCheck { backend: "ms-llama.cpp".into() }]);
    feed(&mut s, 0, Event::BackendDone { backend: "ms-llama.cpp".into(), op: BackendOp::Check, ok: true, message: "looked".into() });
    assert!(s.on_key(key(KeyCode::Char('b'))).is_empty());
    assert!(s.flash.as_ref().unwrap().text.contains("no newer build"), "{:?}", s.flash);

    // The first backend has builds newer: sync sends.
    s.on_key(key(KeyCode::Up));
    assert_eq!(s.on_key(key(KeyCode::Char('b'))), vec![Request::BackendSync { backend: "llama.cpp".into() }]);
    feed(&mut s, 0, Event::BackendDone { backend: "llama.cpp".into(), op: BackendOp::Sync, ok: true, message: "done".into() });

    // Arrow keys and Enter press the focused button; the prompts toggle turns the release prompt off.
    s.on_key(key(KeyCode::Right));
    s.on_key(key(KeyCode::Right));
    s.on_key(key(KeyCode::Right));
    let out = s.on_key(key(KeyCode::Enter));
    assert_eq!(out, vec![Request::BackendDontAsk { backend: "llama.cpp".into(), dont_ask: true }]);
    feed(&mut s, 0, backends("ms-llama.cpp", vec![BackendReport { dont_ask: true, selected: false, ..llama() }, BackendReport { selected: true, ..installed_ms() }]));
    assert!(contains(&render(&s, W, H), "Release prompts: off"));
    let out = s.on_key(key(KeyCode::Char('a')));
    assert_eq!(out, vec![Request::BackendDontAsk { backend: "llama.cpp".into(), dont_ask: false }]);

    s.on_key(key(KeyCode::Esc));
    assert!(s.overlay.is_none());
}

#[test]
fn clicking_a_button_presses_it_and_clicking_outside_closes() {
    let mut s = with("llama.cpp", vec![BackendReport { state: BackendUpdate::UpdateAvailable, latest_release: Some(RemoteTag { tag: "v1.1.0".into(), date: None }), ..llama() }, installed_ms()]);
    open(&mut s);
    let buf = render(&s, W, H);
    let at = find(&buf, "[Upgrade to v1.1.0]");
    let out = click(&mut s, W, H, (at.0 + 3, at.1));
    assert_eq!(out, vec![Request::BackendInstall { backend: "llama.cpp".into() }]);
    // The card's own name moves the cursor to that backend.
    let buf = render(&s, W, H);
    let at = find(&buf, "ms-llama.cpp");
    assert!(click(&mut s, W, H, at).is_empty());
    assert!(matches!(s.overlay, Some(Overlay::Engines { sel: 1, .. })), "{:?}", s.overlay);
    // Outside the panel closes it.
    click(&mut s, W, H, (0, H - 3));
    assert!(s.overlay.is_none());
}

#[test]
fn switching_backend_from_the_library_picker_still_relists_the_catalog() {
    let mut s = up_to_date();
    s.on_key(ctrl('l'));
    // Not in a catalog: nothing is re-listed, but the status line still says what happened.
    let out = feed(&mut s, 0, Event::BackendDone { backend: "ms-llama.cpp".into(), op: BackendOp::Select, ok: true, message: "ms-llama.cpp serves models now".into() });
    assert!(out.is_empty());
    assert_eq!(s.flash.as_ref().unwrap().text, "ms-llama.cpp serves models now");
}

// ---------------------------------------------------------------- the release prompt

fn upgrade_event(warning: Option<&str>) -> Event {
    Event::BackendUpgrade { backend: "llama.cpp".into(), title: "llama.cpp".into(), tag: "v1.1.0".into(), warning: warning.map(String::from) }
}

fn quiet(s: &mut State) {
    s.now_ms += BOOT_FULL_MS + ONBOARD_GUARD_MS + 1;
}

#[test]
fn the_release_prompt_asks_once_with_three_answers() {
    let mut s = up_to_date();
    feed(&mut s, 3, upgrade_event(None));
    assert!(s.overlay.is_none(), "it waits for a quiet moment");
    quiet(&mut s);
    s.on_tick();
    assert_eq!(s.overlay, Some(Overlay::EngineUpgrade));
    let buf = render(&s, W, H);
    let t = dump(&buf);
    assert!(t.contains("A new release of llama.cpp is available (v1.1.0). Upgrade?"), "{t}");
    assert!(t.contains("1 Yes") && t.contains("2 No") && t.contains("3 Don't ask again"), "{t}");
    shot(&s, "engine_upgrade_prompt");

    // After the guard, yes upgrades.
    s.now_ms += UPGRADE_GUARD_MS + 1;
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::BackendInstall { backend: "llama.cpp".into() }]);
    assert!(s.overlay.is_none());
    // A replay of the same release is not asked again.
    feed(&mut s, 4, upgrade_event(None));
    s.on_tick();
    assert!(s.overlay.is_none());
}

#[test]
fn no_and_dont_ask_again() {
    let mut s = up_to_date();
    feed(&mut s, 3, upgrade_event(None));
    quiet(&mut s);
    s.on_tick();
    s.now_ms += UPGRADE_GUARD_MS + 1;
    assert!(s.on_key(key(KeyCode::Char('n'))).is_empty());
    assert!(s.overlay.is_none());
    assert!(s.flash.as_ref().unwrap().text.contains("waiting in the engine window"));

    let mut s = up_to_date();
    feed(&mut s, 3, upgrade_event(Some("ms-v2.0 stops running the Gen 1 MS models: A.")));
    quiet(&mut s);
    s.on_tick();
    assert!(contains(&render(&s, W, H), "! ms-v2.0 stops running the Gen 1 MS models: A."));
    s.now_ms += UPGRADE_GUARD_MS + 1;
    s.on_key(key(KeyCode::Right));
    s.on_key(key(KeyCode::Right));
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::BackendDontAsk { backend: "llama.cpp".into(), dont_ask: true }]);

    // Esc is "not now": it answers no.
    let mut s = up_to_date();
    feed(&mut s, 3, upgrade_event(None));
    quiet(&mut s);
    s.on_tick();
    s.now_ms += UPGRADE_GUARD_MS + 1;
    assert!(s.on_key(key(KeyCode::Esc)).is_empty());
    assert!(s.overlay.is_none() && s.engines.upgrade.is_none());
}

#[test]
fn a_clicked_choice_answers() {
    let mut s = up_to_date();
    feed(&mut s, 3, upgrade_event(None));
    quiet(&mut s);
    s.on_tick();
    let buf = render(&s, W, H);
    let at = find(&buf, "Don't ask again");
    let out = click(&mut s, W, H, at);
    assert_eq!(out, vec![Request::BackendDontAsk { backend: "llama.cpp".into(), dont_ask: true }]);
}

#[test]
fn typing_never_meets_the_prompt() {
    // Someone is typing: it does not appear at all.
    let mut s = up_to_date();
    type_str(&mut s, "ls");
    feed(&mut s, 3, upgrade_event(None));
    quiet(&mut s);
    s.on_tick();
    assert!(s.overlay.is_none());

    // It appears, and a key typed before it could be read goes to the composer; the prompt waits.
    let mut s = up_to_date();
    feed(&mut s, 3, upgrade_event(None));
    quiet(&mut s);
    s.on_tick();
    assert_eq!(s.overlay, Some(Overlay::EngineUpgrade));
    let out = type_str(&mut s, "y");
    assert!(out.is_empty(), "the y was not an answer: {out:?}");
    assert_eq!(s.composer.text(), "y");
    assert!(s.overlay.is_none());
    assert!(s.engines.upgrade.is_some(), "the question is not lost");
}

// ---------------------------------------------------------------- the first-run engine offer

fn first_launch_without_engine() -> State {
    let mut s = state();
    s.start_boot(None);
    welcome(&mut s, 0);
    feed(&mut s, 0, Event::User { name: None, address: None, set: false });
    feed(&mut s, 1, empty_library());
    let missing = BackendReport { state: BackendUpdate::NotInstalled, installed: None, builds_newer: None, last_checked: None, latest_release: None, newest_build: None, ..llama() };
    feed(&mut s, 2, backends("llama.cpp", vec![missing, ms()]));
    s
}

fn answer_name(s: &mut State) {
    s.now_ms += BOOT_FULL_MS + 1;
    s.on_tick();
    s.now_ms += ONBOARD_GUARD_MS + 1;
    type_str(s, "Matthew");
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Enter));
}

#[test]
fn the_engine_is_offered_after_the_name_and_nothing_downloads_without_a_yes() {
    let mut s = first_launch_without_engine();
    answer_name(&mut s);
    let Onboard::Active { step: Step::Engine { backend, bytes, sel, .. }, .. } = s.onboard.clone() else { panic!("{:?}", s.onboard) };
    assert_eq!((backend.as_str(), bytes, sel), ("llama.cpp", 710_000_000, 0));
    let buf = render(&s, W, H);
    let t = dump(&buf);
    assert!(t.contains("Install the llama.cpp engine \u{b7} 710 MB"), "{t}");
    assert!(t.contains("Nothing downloads"), "{t}");
    shot(&s, "onboard_engine");

    // Yes asks the daemon to install it, and the window will show the progress.
    let out = s.on_key(key(KeyCode::Enter));
    assert_eq!(out, vec![Request::BackendInstall { backend: "llama.cpp".into() }]);
    assert!(s.flash.as_ref().unwrap().text.contains("installing llama.cpp"), "{:?}", s.flash);
    assert!(s.engines.progress.contains_key("llama.cpp"));
}

#[test]
fn declining_the_engine_is_remembered_by_the_daemon() {
    let mut s = first_launch_without_engine();
    answer_name(&mut s);
    assert_eq!(s.on_key(key(KeyCode::Char('n'))), vec![Request::BackendDontAsk { backend: "llama.cpp".into(), dont_ask: true }]);
    assert!(s.flash.as_ref().unwrap().text.contains("title bar"), "{:?}", s.flash);
    assert!(!matches!(s.onboard, Onboard::Active { step: Step::Engine { .. }, .. }));

    // Already declined (or installed): the step is not there.
    let mut s = first_launch_without_engine();
    let declined = BackendReport { state: BackendUpdate::NotInstalled, installed: None, dont_ask: true, ..llama() };
    feed(&mut s, 3, backends("llama.cpp", vec![declined, ms()]));
    answer_name(&mut s);
    assert!(!matches!(s.onboard, Onboard::Active { step: Step::Engine { .. }, .. }), "{:?}", s.onboard);
}

#[test]
fn a_known_user_without_an_engine_is_offered_it_too() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 0, Event::User { name: Some("Matthew".into()), address: Some("sir".into()), set: true });
    feed(&mut s, 1, library_with_sonnet());
    let missing = BackendReport { state: BackendUpdate::NotInstalled, installed: None, ..llama() };
    feed(&mut s, 2, backends("llama.cpp", vec![missing, ms()]));
    assert_eq!(s.onboard, Onboard::Pending);
    s.now_ms += BOOT_FULL_MS + 1;
    s.on_tick();
    assert!(matches!(s.onboard, Onboard::Active { step: Step::Engine { .. }, .. }), "{:?}", s.onboard);
    // With the engine installed, a known user is never onboarded.
    let mut s = up_to_date();
    feed(&mut s, 0, Event::User { name: Some("Matthew".into()), address: Some("sir".into()), set: true });
    assert_eq!(s.onboard, Onboard::Idle);
}
