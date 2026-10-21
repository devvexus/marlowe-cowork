//! **The "initializing mainframe" moments** (SPEC 6): the shutdown as the boot in reverse, research
//! channels coming online and a crew run spinning up. Each line appears when the daemon says its
//! step really started or ended, in the order it says so; any key skips; nothing waits on a timer
//! except the timeouts that keep a hung daemon from trapping the user.

mod common;

use common::*;
use marlowe_proto::{Event, PhaseEvent, PhaseKind, PhaseState, Request, RunId};
use marlowe_tui::seq::Farewell;
use marlowe_tui::State;

fn online() -> State {
    let mut s = coding_session();
    feed(&mut s, 0, Event::User { name: Some("Matthew".into()), address: Some("sir".into()), set: true });
    s
}

fn shut(stage: &str) -> PhaseEvent {
    PhaseEvent::start(PhaseKind::Shutdown).stage(stage)
}

fn shut_end(stage: &str, ms: u64) -> PhaseEvent {
    PhaseEvent::end(PhaseKind::Shutdown, ms).stage(stage)
}

/// Quit: the second `^q` stops, and the shutdown starts showing.
fn quit(s: &mut State) -> Request {
    s.on_key(ctrl('q'));
    s.on_key(ctrl('q'));
    assert!(s.quit);
    s.begin_farewell().expect("an online quit that stops the daemon asks it to")
}

#[test]
fn quitting_asks_the_daemon_and_shows_its_close_in_the_order_it_happens() {
    let mut s = online();
    let ask = quit(&mut s);
    assert_eq!(ask, Request::Detach { keep_running: false });
    assert!(s.begin_farewell().is_none(), "asked once");
    assert!(!s.farewell_done(), "waiting for the daemon");

    let mut seq = 10;
    let mut send = |s: &mut State, e: Event| {
        seq += 1;
        feed(s, seq, e);
    };
    send(&mut s, Event::Closing { reason: "the window was closed".into() });
    send(&mut s, Event::Phase(shut("pause")));
    let screen = dump(&render(&s, W, H));
    assert!(screen.contains("pausing crew"), "{screen}");
    send(&mut s, Event::Phase(shut_end("pause", 4).with_done(1).unit("runs")));
    send(&mut s, Event::Phase(shut("unload").detail("dusk-27b")));
    // The unload is on screen, pending, before it ends; the pause above it has its value.
    let screen = dump(&render(&s, W, H));
    let (pause, unload) = (screen.find("pausing crew").unwrap(), screen.find("unloading dusk-27b").unwrap());
    assert!(pause < unload, "lines keep the order the steps began\n{screen}");
    assert!(screen.contains("1 run"), "{screen}");
    send(&mut s, Event::Phase(shut_end("unload", 900).detail("dusk-27b").with_done(13_414).unit("MiB")));
    send(&mut s, Event::Phase(shut("save")));
    send(&mut s, Event::Phase(shut_end("save", 30).with_done(3).unit("sessions")));
    send(&mut s, Event::Phase(shut("journal")));
    send(&mut s, Event::Phase(shut_end("journal", 12)));
    let screen = dump(&render(&s, W, H));
    for want in ["13.1 GB freed", "3 sessions", "sealed"] {
        assert!(screen.contains(want), "{want}\n{screen}");
    }
    assert!(!screen.contains("Goodnight"), "not until the daemon is done\n{screen}");
    assert!(!s.farewell_done());

    send(&mut s, Event::ShuttingDown);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Goodnight, sir."), "the address comes from the profile\n{}", dump(&buf));
    assert!(s.farewell_done() && s.farewell_shut());
    assert_palette_only(&buf);
}

#[test]
fn the_goodnight_uses_the_profile_address_and_never_a_hardcoded_one() {
    let mut s = online();
    feed(&mut s, 0, Event::User { name: Some("Matthew".into()), address: Some("ma'am".into()), set: true });
    assert_eq!(s.goodnight(), "Goodnight, ma'am.");
    feed(&mut s, 0, Event::User { name: None, address: None, set: false });
    assert_eq!(s.goodnight(), "Goodnight.");
}

#[test]
fn a_failed_unload_and_a_missing_byte_count_are_said_honestly() {
    let mut s = online();
    quit(&mut s);
    feed(&mut s, 11, Event::Phase(shut("unload").detail("a")));
    feed(&mut s, 12, Event::Phase(shut_end("unload", 5).detail("a")));
    feed(&mut s, 13, Event::Phase(shut("unload").detail("b")));
    feed(&mut s, 14, Event::Phase(shut_end("unload", 5).detail("failed to unload")));
    let screen = dump(&render(&s, W, H));
    assert!(screen.contains("unloading a") && screen.contains("unloaded"), "no GPU reading, no byte count\n{screen}");
    assert!(screen.contains("failed to unload"), "{screen}");
    assert!(!screen.contains("GB freed"), "{screen}");
}

#[test]
fn any_key_leaves_a_shutdown_at_once() {
    let mut s = online();
    quit(&mut s);
    feed(&mut s, 11, Event::Closing { reason: "x".into() });
    assert!(!s.farewell_done());
    s.on_key(key(crossterm::event::KeyCode::Char('x')));
    assert!(s.farewell_done());
    assert!(!s.farewell_shut(), "a skip is not the daemon finishing");
}

#[test]
fn a_hung_daemon_does_not_trap_the_user() {
    let mut s = online();
    quit(&mut s);
    let began = s.now_ms;
    feed(&mut s, 11, Event::Closing { reason: "x".into() });
    feed(&mut s, 12, Event::Phase(shut("pause")));
    s.now_ms = began + Farewell::HARD_MS - 1;
    assert!(!s.farewell_done(), "it is closing: wait");
    s.now_ms = began + Farewell::HARD_MS;
    assert!(s.farewell_done(), "but not forever");
}

#[test]
fn a_daemon_that_never_starts_closing_is_not_waited_for() {
    // Another window is attached, or it keeps running: no Closing arrives.
    let mut s = online();
    quit(&mut s);
    let began = s.now_ms;
    s.now_ms = began + Farewell::QUIET_MS - 1;
    assert!(!s.farewell_done());
    s.now_ms = began + Farewell::QUIET_MS;
    assert!(s.farewell_done());
    assert!(!s.farewell_shut());
}

#[test]
fn the_connection_ending_is_the_close_finishing() {
    let mut s = online();
    quit(&mut s);
    feed(&mut s, 11, Event::Closing { reason: "x".into() });
    s.on_client(marlowe_tui::ClientEvent::Disconnected { reason: "the daemon closed the connection".into(), attempt: 1 });
    assert!(s.farewell_done() && s.farewell_shut());
}

#[test]
fn quitting_without_a_daemon_or_keeping_it_running_restores_at_once() {
    // Offline: nothing to wait for.
    let mut s = state();
    s.on_key(ctrl('q'));
    assert!(s.quit);
    assert!(s.begin_farewell().is_none());
    assert!(s.farewell_done());
    // Keep running: the daemon is not closing.
    let mut s = online();
    s.closing.keep_on_exit = true;
    s.quit = true;
    assert_eq!(s.exit_request(), Request::Detach { keep_running: true });
    assert!(s.begin_farewell().is_none());
    assert!(s.farewell_done());
}

#[test]
fn the_shutdown_pane_uses_only_palette_inks() {
    let mut s = online();
    quit(&mut s);
    feed(&mut s, 11, Event::Phase(shut("pause")));
    feed(&mut s, 12, Event::Phase(shut_end("pause", 4).with_done(2).unit("runs")));
    feed(&mut s, 13, Event::ShuttingDown);
    for (w, h) in [(140, 40), (120, 30)] {
        let buf = render(&s, w, h);
        assert_palette_only(&buf);
    }
}

// ---- research channels and crew spin-up, in the transcript ----------------------------------------

fn run() -> RunId {
    RunId::from("b8ee1234")
}

fn chan(name: &str, state: PhaseState) -> PhaseEvent {
    PhaseEvent { run: Some(run()), ..PhaseEvent::new(PhaseKind::Channel, state) }.stage(name)
}

#[test]
fn research_channels_come_online_retry_and_finish_in_one_block() {
    let mut s = online();
    feed(&mut s, 20, Event::Phase(chan("arXiv", PhaseState::Start)));
    feed(&mut s, 21, Event::Phase(chan("Semantic Scholar", PhaseState::Start)));
    feed(&mut s, 22, Event::Phase(chan("Crossref", PhaseState::Start)));
    let screen = dump(&render(&s, W, H));
    for c in ["arXiv", "Semantic Scholar", "Crossref"] {
        assert!(screen.contains(c), "{c}\n{screen}");
    }
    feed(&mut s, 23, Event::Phase(chan("arXiv", PhaseState::End).with_done(9)));
    feed(&mut s, 0, Event::Phase(chan("Semantic Scholar", PhaseState::Progress).detail("429 retrying")));
    feed(&mut s, 24, Event::Phase(chan("Crossref", PhaseState::End).with_done(22)));
    feed(&mut s, 25, Event::Phase(chan("arXiv", PhaseState::End).with_done(5)));
    let buf = render(&s, W, H);
    let screen = dump(&buf);
    assert!(screen.contains("14 ✓"), "results add up per channel\n{screen}");
    assert!(screen.contains("22 ✓"), "{screen}");
    assert!(screen.contains("429 retrying ⟳"), "{screen}");
    assert_palette_only(&buf);
    // One block for the run, not one per event.
    let blocks = s.entries.iter().filter(|e| matches!(e, marlowe_tui::state::Entry::Seq(_))).count();
    assert_eq!(blocks, 1);
    feed(&mut s, 26, Event::Phase(chan("Semantic Scholar", PhaseState::End).with_done(7)));
    assert!(contains(&render(&s, W, H), "7 ✓"));
}

#[test]
fn a_failed_channel_is_red_and_says_why() {
    let mut s = online();
    feed(&mut s, 20, Event::Phase(chan("Crossref", PhaseState::Start)));
    feed(&mut s, 21, Event::Phase(chan("Crossref", PhaseState::End).detail("failed: no backend answered")));
    assert!(contains(&render(&s, W, H), "failed: no backend answered"));
}

fn spin(stage: Option<&str>, state: PhaseState) -> PhaseEvent {
    let p = PhaseEvent { run: Some(run()), ..PhaseEvent::new(PhaseKind::Spinup, state) };
    match stage {
        Some(st) => p.stage(st),
        None => p.detail("analyst"),
    }
}

#[test]
fn a_crew_run_spins_up_on_one_line_as_each_step_ends() {
    let mut s = online();
    feed(&mut s, 20, Event::Phase(spin(None, PhaseState::Start)));
    assert!(contains(&render(&s, W, H), "spinning up analyst"));
    feed(&mut s, 21, Event::Phase(PhaseEvent { ms: Some(30), ..spin(Some("sandbox"), PhaseState::End) }));
    let screen = dump(&render(&s, W, H));
    assert!(screen.contains("sandbox ✓") && !screen.contains("model ✓"), "only what has ended\n{screen}");
    feed(&mut s, 22, Event::Phase(spin(Some("tools"), PhaseState::End).with_done(14)));
    feed(&mut s, 23, Event::Phase(spin(Some("budget"), PhaseState::End).detail("600k")));
    feed(&mut s, 24, Event::Phase(spin(Some("model"), PhaseState::End).detail("dusk-27b")));
    let buf = render(&s, W, H);
    let screen = dump(&buf);
    assert!(screen.contains("sandbox ✓ · 14 tools ✓ · budget 600k ✓ · model ✓"), "{screen}");
    assert_palette_only(&buf);
    // The same run spinning up again (resumed after a pause) is a new line.
    feed(&mut s, 25, Event::Phase(spin(None, PhaseState::Start)));
    let blocks = s.entries.iter().filter(|e| matches!(e, marlowe_tui::state::Entry::Seq(_))).count();
    assert_eq!(blocks, 2);
}
