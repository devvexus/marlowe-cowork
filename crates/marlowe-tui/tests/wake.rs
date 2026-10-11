//! The Wake: boot frames, the brief, the voice-driven meter, the Watch section and the footer's Today, toasts and
//! the Watch view. Snapshotted, and every frame keeps the no-background palette rule.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{
    Event, GitInfo, GpuDevice, GpuReport, MemoryStatsReport, Notice, NoticeSource, OllamaReport, QuietHours,
    Request, RunInfo, RunStatus, Severity, VoiceStatus, WatchCandidate, WatchRoot, WatchState,
};
use marlowe_tui::state::Activity;
use marlowe_tui::wake::{Health, Presence, WatchMode};
use marlowe_tui::{Entry, State, View};

const NOW: i64 = 1_800_000_000_000;
const ROOT: &str = "D:/lab/PruningResearch/valiance";

fn run(name: &str, status: RunStatus) -> RunInfo {
    RunInfo {
        dir: format!("{ROOT}/runs/{name}"),
        name: name.into(),
        status,
        stage: None,
        layer: None,
        pid: None,
        heartbeat_ms: None,
        last_line: None,
        step: None,
        loss: None,
        since_ms: NOW - 3_600_000,
    }
}

fn runs() -> Vec<RunInfo> {
    let mut alive = run("marlowe-valiance-22b-wnr5", RunStatus::Alive);
    alive.stage = Some("wnr-p5".into());
    alive.layer = Some(9);
    alive.pid = Some(48_440);
    alive.heartbeat_ms = Some(NOW - 40_000);
    let mut stalled = run("marlowe-valiance-22b-wnr3", RunStatus::Stalled);
    stalled.pid = Some(51_200);
    stalled.heartbeat_ms = Some(NOW - 23 * 60_000);
    let mut finished = run("marlowe-valiance-22b-wnr4", RunStatus::Finished);
    finished.last_line = Some("[05:19:34] done · audit3_marlowe-valiance-22b-wnr4.json written".into());
    let mut failed = run("llama-8b-sweep-b", RunStatus::Failed);
    failed.last_line = Some("torch.OutOfMemoryError: CUDA out of memory.".into());
    vec![finished, alive, failed, stalled]
}

fn watch_state(roots: Vec<WatchRoot>, runs: Vec<RunInfo>) -> WatchState {
    WatchState {
        roots,
        runs,
        gpu: GpuReport {
            devices: vec![GpuDevice {
                index: 0,
                name: "RTX 4090".into(),
                util_pct: 87,
                mem_used_mb: 20_582,
                mem_total_mb: 24_564,
                temp_c: Some(71),
                processes: vec!["python.exe".into()],
            }],
            unavailable: None,
        },
        ollama: OllamaReport { reachable: true, models: 14, new_models: vec![] },
        quiet: QuietHours::default(),
    }
}

fn root() -> WatchRoot {
    WatchRoot {
        path: ROOT.into(),
        added_ms: NOW - 86_400_000,
        runs: 4,
        git: Some(GitInfo {
            branch: "main".into(),
            dirty: 2,
            head: Some("a1b2c3d".into()),
            subject: Some("wnr5: layer schedule from the audit".into()),
            commit_ms: Some(NOW - 7_200_000),
            new_commits: 1,
        }),
        problem: None,
    }
}

fn notice(id: &str, severity: Severity, title: &str, detail: &str, subject: &str) -> Notice {
    Notice {
        id: id.into(),
        at_ms: NOW - 120_000,
        source: NoticeSource::Run,
        severity,
        kind: "k".into(),
        title: title.into(),
        detail: detail.into(),
        subject: subject.into(),
        seen: false,
    }
}

fn offer() -> WatchCandidate {
    WatchCandidate {
        path: "D:/lab/PruningResearch/quill".into(),
        reason: "heartbeat.json, 3 logs, 2 metric files".into(),
        modified_ms: NOW - 2 * 3_600_000,
        live_runs: 1,
    }
}

fn george() -> VoiceStatus {
    VoiceStatus { available: true, enabled: true, voice: Some("George (en-GB)".into()), speaking: false, envelope: vec![] }
}

fn stats() -> MemoryStatsReport {
    MemoryStatsReport {
        beliefs: 390,
        episodes: 41,
        vectors: 388,
        models_loaded: vec![],
        degraded: None,
        journal_seq: 1_204,
    }
}

/// A fresh launch: connected, every probe answered.
fn awake(last_launch: Option<i64>) -> State {
    let mut s = state();
    s.date = "Friday 9 October".into();
    s.start_boot(last_launch);
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 0, Event::MemoryStats(stats()));
    feed(&mut s, 0, Event::WatchState(watch_state(vec![root()], runs())));
    feed(&mut s, 0, Event::VoiceState(george()));
    s
}

fn at(s: &mut State, ms: i64) {
    s.now_ms = NOW + ms;
}

#[test]
fn boot_frames_tick_in_with_real_values() {
    let mut s = awake(None);
    assert!(s.booting());

    let buf = render(&s, W, H);
    assert!(contains(&buf, "MARLOWE"));
    assert!(!contains(&buf, "memory ·"), "no systems line before the meter assembles");
    assert_palette_only(&buf);
    snapshot("boot_0ms", &buf);

    at(&mut s, 400);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "daemon ····· online · v0.1.0"));
    assert!(!contains(&buf, "brains"), "lines tick in one at a time");
    assert_palette_only(&buf);
    snapshot("boot_400ms", &buf);

    at(&mut s, 1_200);
    let buf = render(&s, W, H);
    for line in [
        "memory ····· 390 beliefs · recall online",
        "brains ····· Claude Sonnet 4.5 · 1 model",
        "watch ······ 1 root · 1 run alive · 2 need a look",
        "voice ······ online · George (en-GB)",
    ] {
        assert!(contains(&buf, line), "missing boot line {line}");
    }
    assert_palette_only(&buf);
    snapshot("boot_1200ms", &buf);

    at(&mut s, 1_201);
    assert!(!s.booting(), "the whole boot is at most 1.2 s");
    assert!(!contains(&render(&s, W, H), "any key"));
}

#[test]
fn boot_lines_show_what_has_not_answered() {
    let mut s = state();
    s.start_boot(None);
    welcome(&mut s, 0);
    // The Sentinel and the voice are not running; their probe errors are absorbed, not flashed.
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::wake::NO_SENTINEL.into() });
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::wake::NO_SENTINEL.into() });
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::wake::NO_VOICE.into() });
    assert!(s.flash.is_none());
    assert_eq!(s.wake.sentinel, Presence::Absent);
    let health: Vec<Health> = s.systems().iter().map(|l| l.health).collect();
    assert_eq!(health, vec![Health::Ok, Health::Pending, Health::Pending, Health::Down, Health::Down]);
    at(&mut s, 1_200);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "watch ······ sentinel not running"));
    assert!(contains(&buf, "memory ····· …"));
    assert!(contains(&buf, "sentinel not running"), "the rail says so too");
    assert_palette_only(&buf);

    // An explicit request later is not a probe: that error is shown.
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::wake::NO_SENTINEL.into() });
    assert!(s.flash.is_some());
}

#[test]
fn relaunch_within_half_an_hour_boots_short() {
    let mut s = awake(Some(NOW - 10 * 60_000));
    let b = s.wake.boot.unwrap();
    assert!(b.short);
    at(&mut s, 300);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "voice ······ online · George (en-GB)"), "every line is in by 300 ms");
    assert_palette_only(&buf);
    snapshot("boot_relaunch_300ms", &buf);
    at(&mut s, 301);
    assert!(!s.booting());
}

#[test]
fn keys_typed_during_the_boot_reach_the_composer() {
    let mut s = awake(None);
    at(&mut s, 100);
    assert!(s.booting());
    let reqs = type_str(&mut s, "hi");
    assert!(reqs.is_empty());
    assert!(!s.booting(), "the first key skips the boot");
    assert_eq!(s.composer.text(), "hi", "and is not swallowed");
    // Enter sends straight away.
    let reqs = s.on_key(key(KeyCode::Enter));
    assert!(matches!(&reqs[..], [Request::Ask { text, .. }] if text == "hi"));
}

fn briefed() -> State {
    let mut s = awake(None);
    at(&mut s, 1_500);
    feed(
        &mut s,
        0,
        Event::Brief {
            text: "Good evening, sir. wnr4 finished at 05:19: top1 0.875, kl 0.0803, the best so far. \
                   wnr5 is alive at layer 9. wnr3 has been silent 23 minutes with its pid still alive, \
                   and the 8B sweep ran out of memory."
                .into(),
            spoken: "Good evening, sir. W N R 4 finished.".into(),
            items: vec![],
            offers: vec![offer()],
            session: None,
        },
    );
    s
}

#[test]
fn the_brief_is_the_first_message_and_the_meter_follows_the_voice() {
    let mut s = briefed();
    assert!(matches!(s.entries.first(), Some(Entry::Brief { .. })));
    // The voice speaks it: a loud frame then quiet ones.
    let mut v = george();
    v.speaking = true;
    v.envelope = vec![255, 200, 40, 0, 0, 0, 0, 0, 0, 0];
    feed(&mut s, 0, Event::VoiceState(v));
    assert_eq!(s.activity(), Activity::Speaking);
    at(&mut s, 1_500 + 34);
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Good evening, sir."));
    assert!(contains(&buf, "watch D:/lab/PruningResearch/quill · 1 live run  ^w"));
    assert!(contains(&buf, "speaking"));
    assert_palette_only(&buf);
    snapshot("brief", &buf);
    let meter_now = marlowe_tui::wake::Wake::voice_levels(&s.wake, s.now_ms).unwrap();
    at(&mut s, 1_500 + 200);
    let later = s.wake.voice_levels(s.now_ms).unwrap();
    assert_ne!(meter_now, later, "the meter moves with the envelope");

    // Barge-in: any key stops the voice and still types.
    let reqs = s.on_key(key(KeyCode::Char('o')));
    assert_eq!(reqs, vec![Request::StopSpeaking]);
    assert_eq!(s.composer.text(), "o");
    assert_ne!(s.activity(), Activity::Speaking);

    // A repeat of the same brief is not shown twice; the first ask keeps it on screen.
    let again = s.entries.len();
    let text = match &s.entries[0] {
        Entry::Brief { text, .. } => text.clone(),
        _ => unreachable!(),
    };
    feed(&mut s, 0, Event::Brief { text, spoken: String::new(), items: vec![], offers: vec![], session: None });
    assert_eq!(s.entries.len(), again);
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    assert!(matches!(s.entries.first(), Some(Entry::Brief { .. })));
}

#[test]
fn rail_shows_runs_notices_and_today() {
    let mut s = awake(None);
    s.wake.boot = None;
    feed(
        &mut s,
        0,
        Event::Notices {
            list: vec![
                notice("n1", Severity::Alarm, "8B sweep out of memory", "", &format!("{ROOT}/runs/llama-8b-sweep-b")),
                notice("n2", Severity::Good, "new model marlowe-valiance-22b-wnr4:q4", "", "marlowe-valiance-22b-wnr4:q4"),
                notice("n3", Severity::Attention, "valiance: eval_loss plateau", "5 evals", "D:/lab/other/trainer_state.json"),
            ],
        },
    );
    let buf = render(&s, W, H);
    for want in [
        "Watch",
        "1 alive",
        "✗ llama-8b-sweep-b",
        "◐ marlowe-valiance-22b-wnr3",
        "silent 23m · pid 51200 alive",
        "● marlowe-valiance-22b-wnr5",
        "wnr-p5 · layer 9 · 40s ago",
        "✓ marlowe-valiance-22b-wnr4",
        "▲ valiance: eval_loss plateau",
        "✓ new model marlowe-valiance",
        // Today lives on the footer (the date gives way first when the chords need the room).
        "quiet 23:00–07:00 · 21:40",
    ] {
        assert!(contains(&buf, want), "rail is missing {want}");
    }
    assert!(!contains(&buf, "8B sweep out of memory"), "a notice about a listed run is not listed twice");
    assert_palette_only(&buf);
    snapshot("rail_runs", &buf);
}

#[test]
fn urgent_notices_toast_and_quiet_hours_show_in_the_rail() {
    let mut s = awake(None);
    s.wake.boot = None;
    s.clock = "23:40".into();
    feed(&mut s, 7, Event::Notice(notice("n9", Severity::Alarm, "wnr5 loss is NaN", "step 4,120 · loss nan", &format!("{ROOT}/runs/x"))));
    feed(&mut s, 8, Event::Notice(notice("n8", Severity::Good, "wnr4 best top1", "0.875", "y")));
    assert_eq!(s.wake.toasts.len(), 1, "only Attention and Alarm toast");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "✗ wnr5 loss is NaN"));
    assert!(contains(&buf, "step 4,120 · loss nan"));
    assert!(contains(&buf, "quiet now 23:00–07:00"), "quiet hours are on the footer, amber while in them");
    assert_palette_only(&buf);
    snapshot("toast", &buf);
    // A duplicate at the same severity does not toast again; the toast expires.
    feed(&mut s, 9, Event::Notice(notice("n9", Severity::Alarm, "wnr5 loss is NaN", "", "x")));
    assert_eq!(s.wake.toasts.len(), 1);
    at(&mut s, marlowe_tui::wake::TOAST_MS + 1);
    assert_eq!(s.wake.live_toasts(s.now_ms).count(), 0);
}

#[test]
fn watch_view_offers_discovery_and_adds_only_on_a_key() {
    let mut s = briefed();
    s.wake.boot = None;
    s.now_ms = NOW;
    // ^w with text in the composer deletes a word; with an empty composer it opens the view.
    type_str(&mut s, "two words");
    s.on_key(ctrl('w'));
    assert_eq!(s.composer.text(), "two ");
    s.composer.clear();
    let reqs = s.on_key(ctrl('w'));
    assert_eq!(s.view, View::Watch);
    assert!(reqs.contains(&Request::WatchList));
    assert!(reqs.contains(&Request::WatchCandidates));
    feed(&mut s, 0, Event::WatchCandidates { candidates: vec![offer(), WatchCandidate { path: ROOT.into(), ..offer() }] });
    feed(&mut s, 0, Event::Notices { list: vec![notice("n1", Severity::Attention, "wnr3 stalled", "silent 23m", "z")] });

    let rows = s.wake.rows();
    assert_eq!(rows.len(), 2, "the watched root is not offered again");
    let buf = render(&s, W, H);
    for want in ["ROOTS", "OFFERS", "RUNS", "NOTICES", "D:/lab/PruningResearch/quill", "1 live run", "main · 2 dirty", "gpu RTX 4090 87%", "voice George (en-GB)"] {
        assert!(contains(&buf, want), "watch view is missing {want}");
    }
    assert_palette_only(&buf);
    snapshot("watch_view", &buf);

    // Moving onto the offer and pressing Enter is the consent.
    assert!(s.on_key(key(KeyCode::Up)).is_empty());
    let reqs = s.on_key(key(KeyCode::Down));
    assert!(reqs.is_empty());
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::WatchAdd { path: "D:/lab/PruningResearch/quill".into() }]);

    // Removing a root asks first.
    s.on_key(key(KeyCode::Up));
    assert!(s.on_key(key(KeyCode::Char('x'))).is_empty());
    assert!(matches!(s.wake.mode, WatchMode::ConfirmRemove { .. }));
    assert!(contains(&render(&s, W, H), "nothing on disk is touched"));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::WatchRemove { path: ROOT.into() }]);

    // Adding by path.
    s.on_key(key(KeyCode::Char('a')));
    s.on_paste("\"D:\\lab\\new run\"");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "watch directory › \"D:\\lab\\new run\""));
    assert_palette_only(&buf);
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::WatchAdd { path: "D:\\lab\\new run".into() }]);
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.view, View::Conversation);
}

#[test]
fn palette_reaches_watch_brief_and_voice() {
    let mut s = awake(None);
    s.wake.boot = None;
    s.on_key(ctrl('k'));
    type_str(&mut s, "brief");
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::Brief]);
    s.on_key(ctrl('k'));
    type_str(&mut s, "voice");
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::VoiceSettings { enabled: Some(false), voice: None }]);
}
