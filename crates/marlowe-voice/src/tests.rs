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
}

struct Fake {
    voices: Vec<VoiceInfo>,
    length_s: f32,
    fail: bool,
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
        (Fake { voices, length_s, fail: false, started: None, stopped: false, log: log.clone() }, log)
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
    let ev = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(ev, VoiceEvent::Speaking { id, .. } if id == a));
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
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), VoiceEvent::Speaking { id, .. } if id == c));
    voice.stop();
    assert_eq!(until_done(&rx, c).pop(), Some(VoiceEvent::Spoken { id: c, interrupted: true }));
    assert_eq!(lock(&log).plays, 2);
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
    assert_eq!(again.settings(), VoiceSettings { enabled: false, voice: Some("Hazel (en-GB)".into()) });
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
