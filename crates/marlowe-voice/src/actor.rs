//! The voice actor: one thread owns the backend, synthesizes each utterance (in parts, when the
//! engine streams), plays it and streams its envelope in ~100 ms chunks. An empty `Speaking` marks
//! an utterance accepted before any audio exists. Stop and shutdown bump an epoch; anything queued
//! under an older epoch is dropped (reported as interrupted). Every accepted utterance gets
//! exactly one `Spoken` or `Failed`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

use crate::{envelope, lock, parse_wav, Backend, Part, VoiceEvent, VoiceInfo, VoiceSettings, ENVELOPE_HZ};

/// How often the envelope is streamed.
const TICK: Duration = Duration::from_millis(100);
/// How often the end of a part, or the next part, is looked for.
const FINE_TICK: Duration = Duration::from_millis(20);
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
    /// The voice really speaking while the configured engine cannot ([`Backend::degraded`]).
    pub(crate) degraded: Mutex<Option<String>>,
    /// Why `voice.json` must not be overwritten (fields in it could not be read).
    pub(crate) unsaved: Mutex<Option<String>>,
    /// The engine's load marker ([`Backend::loading_flag`]).
    pub(crate) load: Option<Arc<AtomicU64>>,
}

impl Shared {
    pub(crate) fn new(available: bool, voices: Vec<VoiceInfo>, settings: VoiceSettings) -> Shared {
        Shared {
            available: AtomicBool::new(available),
            voices,
            settings: Mutex::new(settings),
            speaking: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            degraded: Mutex::new(None),
            unsaved: Mutex::new(None),
            load: None,
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

pub(crate) enum Cmd {
    /// `background`: nobody asked for it (a notice); the engine must not load anything for it.
    Speak { id: u64, text: String, voice_id: String, epoch: u64, background: bool },
    Stop,
    /// Start loading the engine (non-blocking).
    Prewarm,
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
        let (id, text, voice_id, epoch, background) = match cmd {
            Cmd::Shutdown => break,
            Cmd::Stop => continue,
            Cmd::Prewarm => {
                backend.prewarm();
                report(backend.as_mut(), &sh, &sink);
                continue;
            }
            Cmd::Speak { id, text, voice_id, epoch, background } => (id, text, voice_id, epoch, background),
        };
        if epoch != sh.epoch() {
            let _ = sink.send(VoiceEvent::Spoken { id, interrupted: true });
            continue;
        }
        sh.speaking.store(true, Ordering::SeqCst);
        let outcome = utter(backend.as_mut(), &rx, &sh, &sink, &mut queue, (id, &text, &voice_id, epoch, background));
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

/// Forwards a degradation the backend reported, and which voice is really speaking.
fn report(backend: &mut dyn Backend, sh: &Shared, sink: &Sender<VoiceEvent>) {
    *lock(&sh.degraded) = backend.degraded();
    if let Some(message) = backend.take_notice() {
        let _ = sink.send(VoiceEvent::Degraded { message });
    }
}

fn utter(
    backend: &mut dyn Backend,
    rx: &Receiver<Cmd>,
    sh: &Shared,
    sink: &Sender<VoiceEvent>,
    queue: &mut VecDeque<Cmd>,
    (id, text, voice_id, epoch, background): (u64, &str, &str, u64, bool),
) -> Outcome {
    if text.trim().is_empty() {
        return Outcome::Done;
    }
    // Clients see the utterance before it is synthesized, so a key press can stop it while a
    // slow engine (a cold voice clone) is still working on it.
    let _ = sink.send(VoiceEvent::Speaking { id, envelope: Vec::new() });
    let stopped = || sh.epoch() != epoch;
    let first = if background {
        backend.begin_background_parts(text, voice_id, &stopped)
    } else {
        backend.begin_parts(text, voice_id, &stopped)
    };
    report(backend, sh, sink);
    let first = match first {
        Ok(p) => p,
        // Given up because of a stop or shutdown.
        Err(_) if stopped() => return Outcome::Interrupted,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let outcome = play_parts(backend, rx, sh, sink, queue, id, first, &stopped);
    if !matches!(outcome, Outcome::Done) {
        backend.abandon();
    }
    outcome
}

/// The next part, if ready within `wait`; a failure as the utterance's outcome.
fn fetch(backend: &mut dyn Backend, sh: &Shared, sink: &Sender<VoiceEvent>, wait: Duration, stopped: &dyn Fn() -> bool) -> Result<Option<Part>, Outcome> {
    let next = backend.next_part(wait, stopped);
    report(backend, sh, sink);
    match next {
        Ok(p) => Ok(p),
        Err(_) if stopped() => Err(Outcome::Interrupted),
        Err(e) => Err(Outcome::Failed(e.to_string())),
    }
}

/// Plays `part` and every part after it, streaming each one's envelope. The next part is
/// fetched while the current one plays; stop and shutdown are answered throughout.
#[allow(clippy::too_many_arguments)]
fn play_parts(
    backend: &mut dyn Backend,
    rx: &Receiver<Cmd>,
    sh: &Shared,
    sink: &Sender<VoiceEvent>,
    queue: &mut VecDeque<Cmd>,
    id: u64,
    part: Part,
    stopped: &dyn Fn() -> bool,
) -> Outcome {
    let mut more = !part.last;
    let mut current = part;
    let mut next: Option<Part> = None;
    loop {
        let wav = match parse_wav(&current.wav) {
            Ok(w) => w,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let env = envelope(&wav, ENVELOPE_HZ);
        let length = Duration::from_millis(wav.duration_ms());
        // A stop that arrived during synthesis wins.
        if stopped() {
            return Outcome::Interrupted;
        }
        if let Err(e) = backend.play(std::mem::take(&mut current.wav)) {
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
            if more && next.is_none() {
                match fetch(backend, sh, sink, Duration::ZERO, stopped) {
                    Ok(Some(p)) => {
                        more = !p.last;
                        next = Some(p);
                    }
                    Ok(None) => {}
                    Err(outcome) => {
                        backend.stop();
                        return outcome;
                    }
                }
            }
            if !backend.is_playing() && (sent >= env.len() || elapsed >= START_GRACE) {
                break;
            }
            if elapsed > length + END_GRACE {
                backend.stop();
                break;
            }
            while next_tick <= Instant::now() {
                next_tick += TICK;
            }
            // Near the end of a part with more to come, look often so the next starts promptly.
            let wake = if (more || next.is_some()) && start.elapsed() + TICK >= length {
                next_tick.min(Instant::now() + FINE_TICK)
            } else {
                next_tick
            };
            match rx.recv_deadline(wake) {
                Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    backend.stop();
                    return Outcome::Shutdown;
                }
                Ok(speak @ Cmd::Speak { .. }) => queue.push_back(speak),
                // Speaking: the engine is warm already.
                Ok(Cmd::Stop | Cmd::Prewarm) | Err(RecvTimeoutError::Timeout) => {}
            }
            if stopped() {
                backend.stop();
                return Outcome::Interrupted;
            }
        }
        // That part has played.
        if let Some(p) = next.take() {
            current = p;
            continue;
        }
        if !more {
            return Outcome::Done;
        }
        // The next part is still being synthesized: wait for it, still answering commands.
        current = loop {
            match fetch(backend, sh, sink, FINE_TICK, stopped) {
                Ok(Some(p)) => {
                    more = !p.last;
                    break p;
                }
                Ok(None) => {}
                Err(outcome) => return outcome,
            }
            loop {
                match rx.try_recv() {
                    Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => return Outcome::Shutdown,
                    Ok(speak @ Cmd::Speak { .. }) => queue.push_back(speak),
                    Ok(Cmd::Stop | Cmd::Prewarm) => {}
                    Err(TryRecvError::Empty) => break,
                }
            }
            if stopped() {
                return Outcome::Interrupted;
            }
        };
    }
}
