//! The voice actor: one thread owns the backend, synthesizes each utterance, plays it and streams
//! its envelope in ~100 ms chunks. Stop and shutdown bump an epoch; anything queued under an older
//! epoch is dropped (reported as interrupted). Every accepted utterance gets exactly one
//! `Spoken` or `Failed`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::{envelope, parse_wav, Backend, VoiceEvent, VoiceInfo, VoiceSettings, ENVELOPE_HZ};

/// How often the envelope is streamed.
const TICK: Duration = Duration::from_millis(100);
/// How long a player may take to report that it is playing.
const START_GRACE: Duration = Duration::from_secs(1);
/// How long past the sound's length a player may keep reporting that it is playing.
const END_GRACE: Duration = Duration::from_secs(2);

/// State the handle and the actor share.
pub(crate) struct Shared {
    pub(crate) available: AtomicBool,
    pub(crate) voices: Vec<VoiceInfo>,
    pub(crate) settings: Mutex<VoiceSettings>,
    pub(crate) speaking: AtomicBool,
    /// Bumped by stop and shutdown.
    pub(crate) epoch: AtomicU64,
}

impl Shared {
    pub(crate) fn new(available: bool, voices: Vec<VoiceInfo>, settings: VoiceSettings) -> Shared {
        Shared {
            available: AtomicBool::new(available),
            voices,
            settings: Mutex::new(settings),
            speaking: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

pub(crate) enum Cmd {
    Speak { id: u64, text: String, voice_id: String, epoch: u64 },
    Stop,
    Shutdown,
}

enum Outcome {
    Done,
    Interrupted,
    Failed(String),
    Shutdown,
}

pub(crate) fn run(mut backend: Box<dyn Backend>, rx: Receiver<Cmd>, sh: Arc<Shared>, sink: Sender<VoiceEvent>) {
    let mut queue: VecDeque<Cmd> = VecDeque::new();
    loop {
        let cmd = match queue.pop_front() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => break,
            },
        };
        let (id, text, voice_id, epoch) = match cmd {
            Cmd::Shutdown => break,
            Cmd::Stop => continue,
            Cmd::Speak { id, text, voice_id, epoch } => (id, text, voice_id, epoch),
        };
        if epoch != sh.epoch() {
            let _ = sink.send(VoiceEvent::Spoken { id, interrupted: true });
            continue;
        }
        sh.speaking.store(true, Ordering::SeqCst);
        let outcome = utter(backend.as_mut(), &rx, &sh, &sink, &mut queue, id, &text, &voice_id, epoch);
        sh.speaking.store(false, Ordering::SeqCst);
        let shutdown = matches!(outcome, Outcome::Shutdown);
        let _ = sink.send(match outcome {
            Outcome::Done => VoiceEvent::Spoken { id, interrupted: false },
            Outcome::Interrupted | Outcome::Shutdown => VoiceEvent::Spoken { id, interrupted: true },
            Outcome::Failed(message) => VoiceEvent::Failed { id, message },
        });
        if shutdown {
            break;
        }
    }
    backend.stop();
    // Whatever is still queued was never spoken.
    for cmd in queue.into_iter().chain(rx.try_iter()) {
        if let Cmd::Speak { id, .. } = cmd {
            let _ = sink.send(VoiceEvent::Spoken { id, interrupted: true });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn utter(
    backend: &mut dyn Backend,
    rx: &Receiver<Cmd>,
    sh: &Shared,
    sink: &Sender<VoiceEvent>,
    queue: &mut VecDeque<Cmd>,
    id: u64,
    text: &str,
    voice_id: &str,
    epoch: u64,
) -> Outcome {
    if text.trim().is_empty() {
        return Outcome::Done;
    }
    let bytes = match backend.synthesize(text, voice_id) {
        Ok(b) => b,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let wav = match parse_wav(&bytes) {
        Ok(w) => w,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let env = envelope(&wav, ENVELOPE_HZ);
    let length = Duration::from_millis(wav.duration_ms());
    // A stop that arrived during synthesis wins.
    if sh.epoch() != epoch {
        return Outcome::Interrupted;
    }
    if let Err(e) = backend.play(bytes) {
        return Outcome::Failed(e.to_string());
    }
    let start = Instant::now();
    let mut next_tick = start;
    let mut sent = 0usize;
    loop {
        let elapsed = start.elapsed();
        let pos_ms = backend.position_ms().unwrap_or(elapsed.as_millis() as u64);
        let due = ((pos_ms * u64::from(ENVELOPE_HZ) / 1000) as usize + 1).min(env.len());
        if due > sent {
            let _ = sink.send(VoiceEvent::Speaking { id, envelope: env[sent..due].to_vec() });
            sent = due;
        }
        if !backend.is_playing() && (sent >= env.len() || elapsed >= START_GRACE) {
            return Outcome::Done;
        }
        if elapsed > length + END_GRACE {
            backend.stop();
            return Outcome::Done;
        }
        while next_tick <= Instant::now() {
            next_tick += TICK;
        }
        match rx.recv_deadline(next_tick) {
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                backend.stop();
                return Outcome::Shutdown;
            }
            Ok(speak @ Cmd::Speak { .. }) => queue.push_back(speak),
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Timeout) => {}
        }
        if sh.epoch() != epoch {
            backend.stop();
            return Outcome::Interrupted;
        }
    }
}
