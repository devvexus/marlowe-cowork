//! Voice actor tests against a fake backend (no audio, no WinRT).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};

use super::*;
use crate::wav::tests::sine16;

#[derive(Default)]
struct Log {
    synth: Vec<(String, String)>,
    plays: usize,
    stops: usize,
    abandons: usize,
}

struct Fake {
    voices: Vec<VoiceInfo>,
    length_s: f32,
    fail: bool,
    /// Synthesis takes this long (giving up when cancelled), like a cold clone.
    slow: Option<Duration>,
    /// Utterances come in this many parts, each `part_delay` after the one before.
    parts: usize,
    part_delay: Duration,
    left: usize,
    next_at: Option<Instant>,
    started: Option<Instant>,
    stopped: bool,
    log: Arc<Mutex<Log>>,
}

impl Fake {
    fn new(length_s: f32) -> (Fake, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let voices = vec![
            info("Hazel", "en-GB", VoiceGender::Female),
            info("George", "en-GB", VoiceGender::Male),
            info("Zira", "en-US", VoiceGender::Female),
        ];
        let fake = Fake {
            voices,
            length_s,
            fail: false,
            slow: None,
            parts: 1,
            part_delay: Duration::ZERO,
            left: 0,
            next_at: None,
            started: None,
            stopped: false,
            log: log.clone(),
        };
        (fake, log)
    }
}

fn info(name: &str, lang: &str, gender: VoiceGender) -> VoiceInfo {
    VoiceInfo { id: format!("id-{name}"), name: name.into(), language: lang.into(), gender }
}

impl Backend for Fake {
    fn voices(&self) -> Vec<VoiceInfo> {
        self.voices.clone()
    }
    fn synthesize(&mut self, text: &str, voice_id: &str) -> Result<Vec<u8>, VoiceError> {
        lock(&self.log).synth.push((text.into(), voice_id.into()));
        if self.fail {
            return Err(VoiceError::Backend("engine fell over".into()));
        }
        Ok(sine16(3000, self.length_s, 0.5, 0.0))
    }
    fn synthesize_until(&mut self, text: &str, voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, VoiceError> {
        if let Some(d) = self.slow {
            let t = Instant::now();
            while t.elapsed() < d {
                if cancelled() {
                    return Err(VoiceError::Stopped);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.synthesize(text, voice_id)
    }
    fn begin_parts(&mut self, text: &str, voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Part, VoiceError> {
        let wav = self.synthesize_until(text, voice_id, cancelled)?;
        self.left = self.parts.saturating_sub(1);
        self.next_at = Some(Instant::now() + self.part_delay);
        Ok(Part { wav, last: self.left == 0 })
    }
    fn next_part(&mut self, wait: Duration, cancelled: &dyn Fn() -> bool) -> Result<Option<Part>, VoiceError> {
        let until = Instant::now() + wait;
        loop {
            if cancelled() {
                return Err(VoiceError::Stopped);
            }
            if self.next_at.is_some_and(|t| Instant::now() >= t) {
                self.left -= 1;
                self.next_at = Some(Instant::now() + self.part_delay);
                return Ok(Some(Part { wav: sine16(3000, self.length_s, 0.5, 0.0), last: self.left == 0 }));
            }
            if Instant::now() >= until {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn abandon(&mut self) {
        lock(&self.log).abandons += 1;
    }
    fn play(&mut self, _wav: Vec<u8>) -> Result<(), VoiceError> {
        lock(&self.log).plays += 1;
        self.started = Some(Instant::now());
        self.stopped = false;
        Ok(())
    }
    fn stop(&mut self) {
        lock(&self.log).stops += 1;
        self.stopped = true;
    }
    fn is_playing(&self) -> bool {
        !self.stopped && self.started.is_some_and(|s| s.elapsed().as_secs_f32() < self.length_s)
    }
}

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-voice-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str, fake: Fake) -> (Voice, Receiver<VoiceEvent>, PathBuf) {
    let home = temp_home(name);
    let (tx, rx) = unbounded();
    (Voice::with_backend(&home, tx, Some(Box::new(fake))), rx, home)
}

/// Events up to and including the terminal one for `id`.
fn until_done(rx: &Receiver<VoiceEvent>, id: u64) -> Vec<VoiceEvent> {
    let mut out = Vec::new();
    loop {
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("voice event");
        let done = matches!(&ev, VoiceEvent::Spoken { id: i, .. } | VoiceEvent::Failed { id: i, .. } if *i == id);
        out.push(ev);
        if done {
            return out;
        }
    }
}

/// Waits for the first audio of `id` (a `Speaking` with frames; the empty one before synthesis
/// does not count).
fn first_audio(rx: &Receiver<VoiceEvent>, id: u64) {
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).expect("voice event") {
            VoiceEvent::Speaking { id: i, envelope } if i == id && !envelope.is_empty() => return,
            VoiceEvent::Spoken { id: i, .. } | VoiceEvent::Failed { id: i, .. } if i == id => panic!("{id} ended before any audio"),
            _ => {}
        }
    }
}

fn frames(events: &[VoiceEvent]) -> usize {
    events
        .iter()
        .map(|e| match e {
            VoiceEvent::Speaking { envelope, .. } => envelope.len(),
            _ => 0,
        })
        .sum()
}

#[test]
fn speaks_with_george_and_streams_the_whole_envelope_in_order() {
    let (fake, log) = Fake::new(0.4);
    let (voice, rx, _home) = start("speak", fake);
    let st = voice.status();
    assert!(st.available && st.enabled && !st.speaking);
    assert_eq!(st.voice.as_deref(), Some("George (en-GB)"));

    let id = voice.speak("Good morning, sir.").unwrap();
    let events = until_done(&rx, id);
    assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: false }));
    let streamed: Vec<u8> = events
        .iter()
        .filter_map(|e| match e {
            VoiceEvent::Speaking { id: i, envelope } if *i == id => Some(envelope.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let expected = envelope(&parse_wav(&sine16(3000, 0.4, 0.5, 0.0)).unwrap(), ENVELOPE_HZ);
    assert_eq!(expected.len(), 12);
    assert_eq!(streamed, expected);
    // Several chunks, not one burst.
    assert!(events.iter().filter(|e| matches!(e, VoiceEvent::Speaking { .. })).count() >= 3);
    assert_eq!(lock(&log).synth, vec![("Good morning, sir.".to_string(), "id-George".to_string())]);
    assert!(!voice.is_speaking());
    voice.shutdown();
}

#[test]
fn stop_interrupts_the_current_utterance_and_drops_the_queue() {
    let (fake, log) = Fake::new(3.0);
    let (voice, rx, _home) = start("stop", fake);
    let a = voice.speak("first").unwrap();
    let b = voice.speak("second").unwrap();
    first_audio(&rx, a);
    assert!(voice.is_speaking());
    let t = Instant::now();
    voice.stop();
    let rest: Vec<_> = until_done(&rx, b).into_iter().filter(|e| !matches!(e, VoiceEvent::Speaking { .. })).collect();
    assert!(t.elapsed() < Duration::from_millis(1000), "barge-in took {:?}", t.elapsed());
    assert_eq!(rest, vec![VoiceEvent::Spoken { id: a, interrupted: true }, VoiceEvent::Spoken { id: b, interrupted: true }]);
    assert_eq!(lock(&log).plays, 1);
    assert!(lock(&log).stops >= 1);
    assert_eq!(lock(&log).synth.len(), 1, "the dropped utterance is never synthesized");

    // The voice keeps working after a barge-in.
    let c = voice.speak("third").unwrap();
    first_audio(&rx, c);
    voice.stop();
    assert_eq!(until_done(&rx, c).pop(), Some(VoiceEvent::Spoken { id: c, interrupted: true }));
    assert_eq!(lock(&log).plays, 2);
    voice.shutdown();
}

#[test]
fn a_stop_while_the_engine_synthesizes_interrupts_before_any_audio() {
    let (mut fake, log) = Fake::new(0.4);
    fake.slow = Some(Duration::from_secs(5));
    let (voice, rx, _home) = start("slowsynth", fake);
    let id = voice.speak("A long thought.").unwrap();
    // Clients hear of the utterance at once, before any audio, so a key press can stop it.
    let ev = rx.recv_timeout(Duration::from_millis(500)).expect("an early Speaking");
    assert_eq!(ev, VoiceEvent::Speaking { id, envelope: Vec::new() });
    assert!(voice.status().speaking);
    let t = Instant::now();
    voice.stop();
    assert_eq!(until_done(&rx, id), vec![VoiceEvent::Spoken { id, interrupted: true }]);
    assert!(t.elapsed() < Duration::from_millis(500), "barge-in took {:?}", t.elapsed());
    assert_eq!(lock(&log).plays, 0, "the stale line is never played");
    assert!(!voice.is_speaking());
    voice.shutdown();
}

#[test]
fn parts_play_back_to_back_and_a_stop_drops_the_rest() {
    let (mut fake, log) = Fake::new(0.2);
    fake.parts = 3;
    // Each part arrives after the one before has played: the actor waits for it.
    fake.part_delay = Duration::from_millis(300);
    let (voice, rx, _home) = start("parts", fake);
    let id = voice.speak("One. Two. Three.").unwrap();
    let events = until_done(&rx, id);
    assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: false }));
    assert_eq!(frames(&events), 18, "three 0.2 s parts at 30 Hz");
    assert_eq!(lock(&log).plays, 3);
    assert_eq!(lock(&log).abandons, 0);

    // A stop between parts ends the utterance and drops what had not come yet.
    let id = voice.speak("Again. And again. And more.").unwrap();
    first_audio(&rx, id);
    voice.stop();
    assert_eq!(until_done(&rx, id).pop(), Some(VoiceEvent::Spoken { id, interrupted: true }));
    assert_eq!(lock(&log).plays, 4);
    assert_eq!(lock(&log).abandons, 1);
    voice.shutdown();
}

#[test]
fn a_voice_json_with_unreadable_fields_is_never_overwritten() {
    let home = temp_home("brokenjson");
    let text = r#"{"enabled": true, "engine": "kokoro", "chatterbox": {"reference": "me.wav"}}"#;
    std::fs::write(VoiceSettings::path(&home), text).unwrap();
    let (fake, _log) = Fake::new(0.2);
    let (tx, _rx) = unbounded();
    let voice = Voice::with_backend(&home, tx, Some(Box::new(fake)));
    let st = voice.configure(Some(false), None).unwrap();
    assert!(!st.enabled, "the change applies");
    assert_eq!(std::fs::read_to_string(VoiceSettings::path(&home)).unwrap(), text, "the file is left as written");
    voice.shutdown();
}

#[test]
fn utterances_queue_without_interrupting() {
    let (fake, _log) = Fake::new(0.2);
    let (voice, rx, _home) = start("queue", fake);
    let a = voice.speak("one").unwrap();
    let b = voice.speak("two").unwrap();
    let events = until_done(&rx, b);
    let terminal: Vec<_> = events.into_iter().filter(|e| matches!(e, VoiceEvent::Spoken { .. })).collect();
    assert_eq!(terminal, vec![VoiceEvent::Spoken { id: a, interrupted: false }, VoiceEvent::Spoken { id: b, interrupted: false }]);
    voice.shutdown();
}

#[test]
fn engine_failure_is_reported_and_blank_text_is_a_no_op() {
    let (mut fake, log) = Fake::new(0.2);
    fake.fail = true;
    let (voice, rx, _home) = start("fail", fake);
    let id = voice.speak("anything").unwrap();
    match until_done(&rx, id).pop() {
        Some(VoiceEvent::Failed { message, .. }) => assert!(message.contains("engine fell over"), "{message}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    let blank = voice.speak("   ").unwrap();
    assert_eq!(until_done(&rx, blank), vec![VoiceEvent::Spoken { id: blank, interrupted: false }]);
    assert_eq!(lock(&log).synth.len(), 1);
    voice.shutdown();
}

#[test]
fn configure_persists_refuses_unknown_and_resets_to_auto() {
    let (fake, _log) = Fake::new(0.2);
    let (voice, _rx, home) = start("configure", fake);

    let st = voice.configure(None, Some("hazel".into())).unwrap();
    assert_eq!(st.voice.as_deref(), Some("Hazel (en-GB)"));
    assert_eq!(VoiceSettings::load(&home).voice.as_deref(), Some("Hazel (en-GB)"));

    match voice.configure(Some(false), Some("Brian".into())) {
        Err(VoiceError::UnknownVoice(n)) => assert_eq!(n, "Brian"),
        other => panic!("expected UnknownVoice, got {:?}", other.map(|s| s.voice)),
    }
    assert!(voice.settings().enabled, "a refused change changes nothing");

    let st = voice.configure(Some(false), None).unwrap();
    assert!(!st.enabled);
    assert!(matches!(voice.speak("hello"), Err(VoiceError::Disabled)));
    voice.shutdown();

    // A fresh voice on the same home reads the saved settings.
    let (fake, _log) = Fake::new(0.2);
    let (tx, _rx2) = unbounded();
    let again = Voice::with_backend(&home, tx, Some(Box::new(fake)));
    assert_eq!(again.settings(), VoiceSettings { enabled: false, voice: Some("Hazel (en-GB)".into()), ..Default::default() });
    let st = again.configure(Some(true), Some("auto".into())).unwrap();
    assert!(st.enabled);
    assert_eq!(st.voice.as_deref(), Some("George (en-GB)"));
    assert_eq!(VoiceSettings::load(&home), VoiceSettings::default());
    again.shutdown();
}

#[test]
fn no_backend_is_unavailable_and_shutdown_closes_the_sink() {
    let home = temp_home("none");
    let (tx, rx) = unbounded();
    let voice = Voice::with_backend(&home, tx, None);
    let st = voice.status();
    assert!(!st.available && st.enabled && st.voice.is_none() && !st.speaking);
    assert!(voice.voices().is_empty());
    assert!(matches!(voice.speak("hello"), Err(VoiceError::Unavailable)));
    voice.stop();
    assert!(matches!(voice.configure(None, Some("George".into())), Err(VoiceError::UnknownVoice(_))));
    assert!(!voice.configure(Some(false), None).unwrap().enabled);
    voice.shutdown();
    voice.shutdown();
    assert!(rx.recv_timeout(Duration::from_secs(1)).is_err(), "sink closed");
}

#[test]
fn a_backend_without_voices_is_unavailable() {
    let (mut fake, _log) = Fake::new(0.2);
    fake.voices.clear();
    let (voice, rx, _home) = start("novoices", fake);
    assert!(!voice.status().available);
    assert!(matches!(voice.speak("hello"), Err(VoiceError::Unavailable)));
    voice.shutdown();
    assert!(rx.recv_timeout(Duration::from_secs(1)).is_err());
}

#[test]
fn shutdown_mid_utterance_joins_reports_and_closes_the_sink() {
    let (fake, _log) = Fake::new(3.0);
    let (voice, rx, _home) = start("shutdown", fake);
    let id = voice.speak("long").unwrap();
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), VoiceEvent::Speaking { .. }));
    let t = Instant::now();
    let clone = voice.clone();
    clone.shutdown();
    voice.shutdown();
    assert!(t.elapsed() < Duration::from_secs(1));
    let mut rest = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(VoiceEvent::Speaking { .. }) => {}
            Ok(ev) => rest.push(ev),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(e) => panic!("sink not closed after shutdown: {e:?}"),
        }
    }
    assert_eq!(rest, vec![VoiceEvent::Spoken { id, interrupted: true }]);
    assert!(matches!(voice.speak("after"), Err(VoiceError::Stopped)));
}

#[test]
fn render_synthesizes_to_a_buffer_without_playing() {
    let (mut fake, log) = Fake::new(0.5);
    let r = render(&mut fake, "Good morning, sir.", Some("zira")).unwrap();
    assert_eq!(r.voice.name, "Zira");
    assert_eq!(r.wav.duration_ms(), 500);
    assert_eq!(r.envelope.len(), 15);
    assert_eq!(lock(&log).plays, 0);
    let mut empty = Fake::new(0.5).0;
    empty.voices.clear();
    assert!(matches!(render(&mut empty, "x", None), Err(VoiceError::Unavailable)));
}

#[test]
fn prewarm_is_a_no_op_for_winrt_and_when_off() {
    let (fake, log) = Fake::new(0.2);
    let (voice, rx, _home) = start("prewarm", fake);
    voice.prewarm();
    voice.configure(Some(false), None).unwrap();
    voice.prewarm();
    voice.shutdown();
    assert!(rx.recv_timeout(Duration::from_secs(1)).is_err(), "no events");
    assert!(lock(&log).synth.is_empty());
}

#[test]
fn engine_check_names_what_is_missing() {
    let mut s = VoiceSettings::default();
    assert!(engine_check(&s).unwrap().contains("WinRT"));
    s.engine = Engine::Chatterbox;
    assert!(engine_check(&s).unwrap_err().contains("no python"));
    let home = temp_home("check");
    let sample = home.join("me.wav");
    std::fs::write(&sample, b"RIFF").unwrap();
    s.chatterbox = Some(ChatterboxSettings { python: Some("python".into()), reference: Some(home.join("nope.wav")), ..Default::default() });
    assert!(engine_check(&s).unwrap_err().contains("nope.wav"));
    s.chatterbox.as_mut().unwrap().reference = Some(sample);
    assert!(engine_check(&s).unwrap().contains("placeholder"));
    s.chatterbox.as_mut().unwrap().python = Some(home.join("missing").join("python.exe"));
    assert!(engine_check(&s).unwrap_err().contains("does not exist"));
}

/// The Chatterbox engine against `tests/fake_sidecar.py` (python on PATH; no model, no GPU) with
/// the fake player above. Windows only: the sidecar runs in a Job Object.
#[cfg(windows)]
mod chatterbox {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;
    use crate::sidecar::SidecarConfig;
    use crate::VramCheck;

    fn have_python() -> bool {
        let ok = Command::new("python").arg("--version").output().is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("python is not on PATH: skipped");
        }
        ok
    }

    fn fake_cfg(home: &Path, mode: &str) -> SidecarConfig {
        let reference = home.join("ref.wav");
        std::fs::write(&reference, b"RIFF").unwrap();
        SidecarConfig {
            python: Some("python".into()),
            script: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fake_sidecar.py")),
            reference: Some(reference),
            cache_dir: home.join("cache").join("voice"),
            args: vec!["--idle".into(), "30".into()],
            scrub: vec!["OPENROUTER_API_KEY".into()],
            env: vec![("FAKE_SIDECAR_MODE".into(), mode.into())],
            load_timeout: Duration::from_secs(20),
            ready_grace: Duration::from_secs(20),
            request_timeout: Duration::from_secs(10),
            per_char: Duration::ZERO,
            vram_mib: 4_608.0,
            vram: None,
        }
    }

    type Started = (Voice, Receiver<VoiceEvent>, Arc<Mutex<Log>>, PathBuf);

    fn clone_voice(name: &str, cfg: impl FnOnce(&Path) -> SidecarConfig) -> Started {
        let home = temp_home(name);
        let (player, log) = Fake::new(0.2);
        let backend = ChatterboxBackend::new(Box::new(player), cfg(&home), None);
        let (tx, rx) = unbounded();
        (Voice::with_backend(&home, tx, Some(Box::new(backend))), rx, log, home)
    }

    fn degraded(events: &[VoiceEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                VoiceEvent::Degraded { message } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn spawns(home: &Path) -> usize {
        std::fs::read_to_string(home.join("cache").join("voice").join("spawns.log")).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn wait_for(mut done: impl FnMut() -> bool, limit: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        done()
    }

    fn first_spawn(home: &Path) -> u32 {
        let log = std::fs::read_to_string(home.join("cache").join("voice").join("spawns.log")).unwrap();
        log.lines().next().unwrap().trim().parse().unwrap()
    }

    #[test]
    fn the_clone_speaks_through_the_sidecar_and_the_player_plays_it() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-ok", |h| fake_cfg(h, "ok"));
        assert_eq!(voice.status().voice.as_deref(), Some("Chatterbox clone (en)"));
        assert_eq!(voice.voices().len(), 1);
        for round in 0..2 {
            let id = voice.speak("Good morning, sir. Systems are online.").unwrap();
            let events = until_done(&rx, id);
            assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: false }), "round {round}: {events:?}");
            assert!(degraded(&events).is_empty(), "{events:?}");
            // One part per sentence, each the sidecar's 300 ms tone, streamed at 30 Hz.
            assert_eq!(frames(&events), 18);
        }
        assert!(lock(&log).synth.is_empty(), "WinRT synthesized nothing");
        assert_eq!(lock(&log).plays, 4, "two parts per utterance, played as they came");
        assert_eq!(voice.status().voice.as_deref(), Some("Chatterbox clone (en)"));
        assert_eq!(spawns(&home), 1, "one sidecar serves every utterance");
        let left: Vec<String> = std::fs::read_dir(home.join("cache").join("voice"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("utt-"))
            .collect();
        assert!(left.is_empty(), "handed-over WAVs are deleted: {left:?}");
        voice.shutdown();
    }

    #[test]
    fn a_sidecar_that_will_not_load_falls_back_to_winrt_and_says_why_once() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-fatal", |h| fake_cfg(h, "fatal"));
        let a = voice.speak("Hello there, sir.").unwrap();
        let events = until_done(&rx, a);
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id: a, interrupted: false }));
        let d = degraded(&events);
        assert_eq!(d.len(), 1, "{events:?}");
        assert!(d[0].contains("not importable") && d[0].contains("Windows voice"), "{}", d[0]);
        assert_eq!(lock(&log).synth, vec![("Hello there, sir.".to_string(), "id-George".to_string())]);
        // Clients see who is really speaking.
        assert_eq!(voice.status().voice.as_deref(), Some("George (en-GB) - clone unavailable"));
        // Cooling down: WinRT speaks at once, nothing is respawned, nothing is re-reported.
        let b = voice.speak("Again.").unwrap();
        assert!(degraded(&until_done(&rx, b)).is_empty());
        assert_eq!(spawns(&home), 1);
        assert_eq!(lock(&log).synth.len(), 2);
        voice.shutdown();
    }

    #[test]
    fn a_missing_interpreter_or_a_crash_while_loading_falls_back_with_the_reason() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, _home) = clone_voice("cb-nopython", |h| SidecarConfig {
            python: Some("definitely-not-python-marlowe.exe".into()),
            ..fake_cfg(h, "ok")
        });
        let id = voice.speak("Hello.").unwrap();
        let events = until_done(&rx, id);
        assert!(degraded(&events).first().is_some_and(|m| m.contains("could not run")), "{events:?}");
        assert_eq!(lock(&log).synth.len(), 1);
        voice.shutdown();

        let (voice, rx, log, _home) = clone_voice("cb-crash", |h| fake_cfg(h, "crash"));
        let id = voice.speak("Hello.").unwrap();
        let events = until_done(&rx, id);
        let d = degraded(&events);
        assert!(d.first().is_some_and(|m| m.contains("exited while loading") && m.contains("out of memory")), "{events:?}");
        assert_eq!(lock(&log).synth.len(), 1);
        voice.shutdown();
    }

    #[test]
    fn a_sidecar_that_dies_mid_request_is_respawned_once() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-die", |h| fake_cfg(h, "die-once"));
        let id = voice.speak("Still here?").unwrap();
        let events = until_done(&rx, id);
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: false }));
        assert!(degraded(&events).is_empty(), "{events:?}");
        assert!(lock(&log).synth.is_empty());
        assert_eq!(spawns(&home), 2);
        voice.shutdown();
    }

    #[test]
    fn an_idle_exit_is_replaced_by_a_fresh_sidecar() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) =
            clone_voice("cb-idle", |h| SidecarConfig { args: vec!["--idle".into(), "1".into()], ..fake_cfg(h, "ok") });
        let a = voice.speak("One.").unwrap();
        until_done(&rx, a);
        std::thread::sleep(Duration::from_millis(1_800));
        let b = voice.speak("Two.").unwrap();
        let events = until_done(&rx, b);
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id: b, interrupted: false }));
        assert!(degraded(&events).is_empty(), "{events:?}");
        assert!(lock(&log).synth.is_empty());
        assert_eq!(spawns(&home), 2);
        voice.shutdown();
    }

    #[test]
    fn a_hung_request_times_out_and_a_refused_line_falls_back_alone() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, _home) =
            clone_voice("cb-hang", |h| SidecarConfig { request_timeout: Duration::from_secs(1), ..fake_cfg(h, "hang") });
        let t = Instant::now();
        let id = voice.speak("Anyone?").unwrap();
        let events = until_done(&rx, id);
        assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
        assert!(degraded(&events).first().is_some_and(|m| m.contains("did not answer")), "{events:?}");
        assert_eq!(lock(&log).synth.len(), 1);
        voice.shutdown();

        // An error for one line is not a broken sidecar: the same process is asked again. A
        // second refusal in a row is: it is dropped (freeing its VRAM) and left alone.
        let (voice, rx, log, home) = clone_voice("cb-error", |h| fake_cfg(h, "error"));
        let a = voice.speak("Odd text.").unwrap();
        let d = degraded(&until_done(&rx, a));
        assert!(d.first().is_some_and(|m| m.contains("text too strange")), "{d:?}");
        let pid = first_spawn(&home);
        assert!(owns_process(pid), "kept after one refusal");
        let b = voice.speak("Odd text.").unwrap();
        assert!(degraded(&until_done(&rx, b)).is_empty(), "the same cause is reported once");
        assert!(!owns_process(pid), "dropped after the second");
        let c = voice.speak("Odd text.").unwrap();
        assert!(degraded(&until_done(&rx, c)).is_empty());
        assert_eq!(spawns(&home), 1, "cooling: not respawned");
        assert_eq!(lock(&log).synth.len(), 3);
        voice.shutdown();
    }

    #[test]
    fn out_of_memory_drops_the_sidecar_at_once() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-oom", |h| fake_cfg(h, "oom"));
        let a = voice.speak("Hello there.").unwrap();
        let d = degraded(&until_done(&rx, a));
        assert!(d.len() == 1 && d[0].contains("OutOfMemoryError"), "{d:?}");
        assert!(!owns_process(first_spawn(&home)), "dropped on the first OOM, freeing its VRAM");
        let b = voice.speak("Hello again.").unwrap();
        assert!(degraded(&until_done(&rx, b)).is_empty());
        assert_eq!(spawns(&home), 1);
        assert_eq!(lock(&log).synth.len(), 2);
        assert!(voice.status().voice.is_some_and(|v| v.ends_with("clone unavailable")));
        voice.shutdown();
    }

    #[test]
    fn a_prewarmed_sidecar_that_failed_to_load_is_explained_not_loaded_again() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-prewarm-fatal", |h| fake_cfg(h, "fatal"));
        voice.prewarm();
        assert!(wait_for(|| spawns(&home) == 1, Duration::from_secs(10)));
        // It reports {fatal} to nobody and exits (the fake does so at once after starting).
        std::thread::sleep(Duration::from_millis(1_500));
        let id = voice.speak("Hello.").unwrap();
        let d = degraded(&until_done(&rx, id));
        assert!(d.len() == 1 && d[0].contains("not importable"), "{d:?}");
        assert_eq!(spawns(&home), 1, "the reason is read from the exited sidecar");
        assert_eq!(lock(&log).synth.len(), 1);
        voice.shutdown();
    }

    #[test]
    fn a_line_during_a_slow_load_is_said_by_winrt_and_the_load_carries_on() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-grace", |h| SidecarConfig {
            ready_grace: Duration::from_millis(300),
            env: vec![("FAKE_SIDECAR_MODE".into(), "slowload".into()), ("FAKE_SIDECAR_LOAD".into(), "3".into())],
            ..fake_cfg(h, "slowload")
        });
        let t = Instant::now();
        let a = voice.speak("Hello there, sir.").unwrap();
        let events = until_done(&rx, a);
        assert!(t.elapsed() < Duration::from_secs(2), "waited {:?}", t.elapsed());
        assert!(degraded(&events).is_empty(), "loading is not a failure: {events:?}");
        assert_eq!(lock(&log).synth.len(), 1, "WinRT said it");
        assert_eq!(voice.status().voice.as_deref(), Some("Chatterbox clone (en)"));
        std::thread::sleep(Duration::from_secs(4));
        let b = voice.speak("And now?").unwrap();
        assert_eq!(until_done(&rx, b).pop(), Some(VoiceEvent::Spoken { id: b, interrupted: false }));
        assert_eq!(lock(&log).synth.len(), 1, "the clone, loaded meanwhile, said the second");
        assert_eq!(spawns(&home), 1);
        voice.shutdown();
    }

    #[test]
    fn a_sidecar_dying_mid_utterance_hands_the_rest_to_winrt() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, _home) = clone_voice("cb-diemid", |h| fake_cfg(h, "die-mid"));
        let id = voice.speak("Good morning, sir. Systems are online.").unwrap();
        let events = until_done(&rx, id);
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: false }), "{events:?}");
        assert!(degraded(&events).first().is_some_and(|m| m.contains("exited mid-utterance")), "{events:?}");
        assert_eq!(lock(&log).synth, vec![("Systems are online.".to_string(), "id-George".to_string())], "only what the clone had not said");
        assert_eq!(lock(&log).plays, 2);
        voice.shutdown();
    }

    #[test]
    fn stop_during_synthesis_interrupts_at_once_and_cancels_the_request() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-stop", |h| fake_cfg(h, "slow"));
        // Warm first, so the stop lands mid-request rather than mid-load.
        voice.prewarm();
        assert!(wait_for(|| spawns(&home) == 1, Duration::from_secs(10)));
        let id = voice.speak("A long thought.").unwrap();
        std::thread::sleep(Duration::from_millis(700));
        let t = Instant::now();
        voice.stop();
        let events = until_done(&rx, id);
        assert!(t.elapsed() < Duration::from_millis(1_000), "barge-in took {:?}", t.elapsed());
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: true }));
        assert!(degraded(&events).is_empty() && lock(&log).synth.is_empty() && lock(&log).plays == 0);
        let cancels = home.join("cache").join("voice").join("cancels.log");
        assert!(wait_for(|| cancels.exists(), Duration::from_secs(3)), "the sidecar was told to cancel");
        // The sidecar is still usable after a cancel (its stale reply is ignored).
        let next = voice.speak("Short.").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        voice.stop();
        assert_eq!(until_done(&rx, next).pop(), Some(VoiceEvent::Spoken { id: next, interrupted: true }));
        assert_eq!(spawns(&home), 1);
        voice.shutdown();
    }

    #[test]
    fn shutdown_while_the_model_loads_does_not_wait_for_it() {
        if !have_python() {
            return;
        }
        let (voice, rx, _log, home) = clone_voice("cb-slowload", |h| fake_cfg(h, "slowload"));
        let id = voice.speak("Hello.").unwrap();
        assert!(wait_for(|| spawns(&home) == 1, Duration::from_secs(10)));
        let t = Instant::now();
        voice.shutdown();
        assert!(t.elapsed() < Duration::from_secs(2), "shutdown took {:?}", t.elapsed());
        let mut terminal = Vec::new();
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
            if !matches!(ev, VoiceEvent::Speaking { .. }) {
                terminal.push(ev);
            }
        }
        assert_eq!(terminal, vec![VoiceEvent::Spoken { id, interrupted: true }]);
    }

    #[test]
    fn prewarm_starts_the_sidecar_and_its_whole_tree_is_marlowes_own() {
        if !have_python() {
            return;
        }
        let (voice, rx, _log, home) = clone_voice("cb-own", |h| fake_cfg(h, "child"));
        let t = Instant::now();
        voice.prewarm();
        assert!(t.elapsed() < Duration::from_millis(100), "prewarm never blocks");
        let pids = home.join("cache").join("voice").join("pids.txt");
        assert!(wait_for(|| std::fs::read_to_string(&pids).is_ok_and(|s| s.ends_with('\n')), Duration::from_secs(10)));
        let text = std::fs::read_to_string(&pids).unwrap();
        let ids: Vec<u32> = text.split_whitespace().map(|p| p.parse().unwrap()).collect();
        let (sidecar, child) = (ids[0], ids[1]);
        assert!(owns_process(sidecar), "the sidecar");
        assert!(owns_process(child), "a process the sidecar started (the venv launcher's interpreter)");
        assert!(!owns_process(std::process::id()), "the daemon itself is not the sidecar");
        // A training job: some other python, outside the sidecar's job.
        let mut other = Command::new("python").args(["-c", "import time; time.sleep(30)"]).spawn().unwrap();
        assert!(!owns_process(other.id()), "foreign compute stays foreign");
        let _ = other.kill();
        let _ = other.wait();

        let id = voice.speak("Warm already.").unwrap();
        assert_eq!(until_done(&rx, id).pop(), Some(VoiceEvent::Spoken { id, interrupted: false }));
        assert_eq!(spawns(&home), 1, "speaking used the prewarmed sidecar");
        voice.shutdown();
        assert!(!owns_process(sidecar) && !owns_process(child), "the tree is gone with the voice");
    }

    /// A notice never loads the clone: with no sidecar up, the Windows voice says it, nothing
    /// starts and nothing is reported. Once the clone is up (a line the user asked for loaded
    /// it), a notice uses it.
    #[test]
    fn a_background_line_never_starts_the_clone() {
        if !have_python() {
            return;
        }
        let (voice, rx, log, home) = clone_voice("cb-background", |h| fake_cfg(h, "ok"));
        let a = voice.speak_background("A research plan is waiting.").unwrap();
        let events = until_done(&rx, a);
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id: a, interrupted: false }));
        assert!(degraded(&events).is_empty(), "{events:?}");
        assert_eq!(lock(&log).synth.len(), 1, "the Windows voice said it");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(spawns(&home), 0, "no sidecar started for a notice");

        let b = voice.speak("Good morning.").unwrap();
        until_done(&rx, b);
        assert_eq!(spawns(&home), 1);
        let c = voice.speak_background("Another plan is waiting.").unwrap();
        until_done(&rx, c);
        assert_eq!(lock(&log).synth.len(), 1, "the loaded clone said the second notice");
        assert_eq!(spawns(&home), 1);
        voice.shutdown();
    }

    /// The clone loads onto the GPU only when its footprint fits: otherwise neither a line nor a
    /// prewarm starts it, the Windows voice speaks, and the reason is said once.
    #[test]
    fn the_clone_does_not_start_on_a_gpu_without_room_for_it() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let full = VramCheck(Arc::new(move |need| {
            lock(&seen).push(need);
            Some(false)
        }));
        let (voice, rx, log, home) =
            clone_voice("cb-vram", |h| SidecarConfig { vram: Some(full), args: vec!["--device".into(), "auto".into()], ..fake_cfg(h, "ok") });
        voice.prewarm();
        let id = voice.speak("Hello.").unwrap();
        let events = until_done(&rx, id);
        assert_eq!(events.last(), Some(&VoiceEvent::Spoken { id, interrupted: false }));
        let d = degraded(&events);
        assert!(d.iter().any(|m| m.contains("not enough free GPU memory")), "{events:?}");
        assert_eq!(lock(&log).synth.len(), 1);
        assert_eq!(spawns(&home), 0);
        assert_eq!(lock(&asked).first(), Some(&4_608.0));
        voice.shutdown();

        // On the CPU there is nothing to check.
        let never = VramCheck(Arc::new(|_| Some(false)));
        let cfg = |h: &Path| SidecarConfig { vram: Some(never), args: vec!["--device".into(), "cpu".into()], ..fake_cfg(h, "ok") };
        assert!(cfg(&temp_home("cb-vram-cpu")).vram_gate().is_ok());
    }

    #[test]
    fn the_built_in_script_is_written_to_the_cache_and_an_early_exit_is_explained() {
        if !have_python() {
            return;
        }
        // The real sidecar with an argument it refuses: it exits before importing anything heavy.
        let (voice, rx, log, home) = clone_voice("cb-real", |h| SidecarConfig {
            script: None,
            args: vec!["--device".into(), "bogus".into()],
            ..fake_cfg(h, "ok")
        });
        let id = voice.speak("Hello.").unwrap();
        let events = until_done(&rx, id);
        let d = degraded(&events);
        assert!(d.first().is_some_and(|m| m.contains("exited while loading") && m.contains("invalid choice")), "{events:?}");
        assert_eq!(lock(&log).synth.len(), 1);
        let copy = std::fs::read_to_string(home.join("cache").join("voice").join("chatterbox_sidecar.py")).unwrap();
        assert!(copy.contains("def split(text):") && copy.contains("\"ready\": True"));
        voice.shutdown();
    }
}
