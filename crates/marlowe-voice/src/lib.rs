//! Spoken voice for the Wake (M3): WinRT `SpeechSynthesizer` on Windows, nothing elsewhere.
//!
//! The daemon owns one [`Voice`]: an actor thread that synthesizes text to WAV, computes an RMS
//! amplitude envelope at [`ENVELOPE_HZ`] for the presence meter, plays it, and reports
//! [`VoiceEvent`]s through a channel. [`Voice::stop`] is barge-in. Settings (on/off, voice name)
//! persist in `<home>\voice.json`. With no backend the handle still works and reports itself
//! unavailable; nothing is an error the caller must handle to keep going.
//!
//! The full voice (Kokoro, push-to-talk) replaces the backend in M6 as a sidecar.

#![forbid(unsafe_code)]

mod actor;
mod select;
mod settings;
mod wav;
#[cfg(windows)]
mod winrt;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crossbeam_channel::Sender;

use actor::{Cmd, Shared};

pub use marlowe_proto::{VoiceStatus, ENVELOPE_HZ};
pub use select::pick_voice;
pub use settings::VoiceSettings;
pub use wav::{envelope, parse_wav, Wav};
#[cfg(windows)]
pub use winrt::WinRtBackend;

/// A voice's reported gender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VoiceGender {
    Male,
    Female,
    Unknown,
}

/// An installed voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceInfo {
    /// Backend id (stable across runs).
    pub id: String,
    /// Short name: `George`.
    pub name: String,
    /// BCP-47: `en-GB`.
    pub language: String,
    pub gender: VoiceGender,
}

impl VoiceInfo {
    /// `George (en-GB)`.
    pub fn display(&self) -> String {
        format!("{} ({})", self.name, self.language)
    }
}

/// What the voice actor reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    /// Playback of utterance `id` began or continues; `envelope` is the next chunk of RMS
    /// frames (0..=255 at [`ENVELOPE_HZ`]).
    Speaking { id: u64, envelope: Vec<u8> },
    /// Utterance `id` ended; `interrupted` after [`Voice::stop`].
    Spoken { id: u64, interrupted: bool },
    /// Utterance `id` could not be spoken.
    Failed { id: u64, message: String },
}

/// A voice failure.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("no speech backend on this machine")]
    Unavailable,
    #[error("voice is turned off")]
    Disabled,
    #[error("no installed voice matches '{0}'")]
    UnknownVoice(String),
    #[error("bad wav: {0}")]
    BadWav(String),
    #[error("speech backend: {0}")]
    Backend(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("the voice has stopped")]
    Stopped,
}

/// A speech engine. Runs on the voice actor's thread.
pub trait Backend: Send {
    /// Installed voices.
    fn voices(&self) -> Vec<VoiceInfo>;
    /// Synthesizes `text` with voice `voice_id` to WAV bytes.
    fn synthesize(&mut self, text: &str, voice_id: &str) -> Result<Vec<u8>, VoiceError>;
    /// Starts playing WAV bytes; returns once playback has started.
    fn play(&mut self, wav: Vec<u8>) -> Result<(), VoiceError>;
    /// Stops playback now (barge-in). Idempotent.
    fn stop(&mut self);
    fn is_playing(&self) -> bool;
    /// Playback position of the current sound, when the engine reports one. The envelope follows
    /// it; without it the envelope follows the wall clock from [`Backend::play`].
    fn position_ms(&self) -> Option<u64> {
        None
    }
}

/// The platform backend: WinRT on Windows (when the speech runtime answers), else `None`.
pub fn default_backend() -> Option<Box<dyn Backend>> {
    #[cfg(windows)]
    {
        WinRtBackend::new().ok().map(|b| Box::new(b) as Box<dyn Backend>)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// A synthesized utterance, not played.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub voice: VoiceInfo,
    /// The WAV file as the engine produced it.
    pub bytes: Vec<u8>,
    pub wav: Wav,
    /// RMS frames at [`ENVELOPE_HZ`].
    pub envelope: Vec<u8>,
}

/// Synthesizes `text` with the voice [`pick_voice`] chooses for `preferred`, to a buffer.
/// Nothing is played.
pub fn render(backend: &mut dyn Backend, text: &str, preferred: Option<&str>) -> Result<Rendered, VoiceError> {
    let voices = backend.voices();
    let voice = pick_voice(&voices, preferred).cloned().ok_or(VoiceError::Unavailable)?;
    let bytes = backend.synthesize(text, &voice.id)?;
    let wav = parse_wav(&bytes)?;
    let envelope = envelope(&wav, ENVELOPE_HZ);
    Ok(Rendered { voice, bytes, wav, envelope })
}

/// Handle over the voice actor. Clones share it.
#[derive(Clone)]
pub struct Voice {
    inner: Arc<Inner>,
}

struct Inner {
    home: PathBuf,
    shared: Arc<Shared>,
    /// `None` without a backend: there is no actor.
    tx: Option<Sender<Cmd>>,
    join: Mutex<Option<JoinHandle<()>>>,
    /// Held only when there is no actor, so [`Voice::shutdown`] still closes the sink.
    idle_sink: Mutex<Option<Sender<VoiceEvent>>>,
    next_id: AtomicU64,
    down: AtomicBool,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Names that reset the voice choice to automatic.
fn is_auto(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n.eq_ignore_ascii_case("auto") || n.eq_ignore_ascii_case("default")
}

impl Voice {
    /// Starts the actor with [`default_backend`] and the settings in `<home>\voice.json`.
    /// Never fails: without a backend the voice reports itself unavailable.
    pub fn start(home: &Path, sink: Sender<VoiceEvent>) -> Voice {
        Self::with_backend(home, sink, default_backend())
    }

    /// [`Voice::start`] with an explicit backend (tests pass a fake, or `None`).
    pub fn with_backend(home: &Path, sink: Sender<VoiceEvent>, backend: Option<Box<dyn Backend>>) -> Voice {
        let voices = backend.as_ref().map(|b| b.voices()).unwrap_or_default();
        // A backend with no installed voices cannot speak.
        let backend = backend.filter(|_| !voices.is_empty());
        let shared = Arc::new(Shared::new(backend.is_some(), voices, VoiceSettings::load(home)));
        let (mut tx, mut join, mut idle_sink) = (None, None, Some(sink.clone()));
        if let Some(backend) = backend {
            let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
            let sh = shared.clone();
            let spawned = std::thread::Builder::new()
                .name("marlowe-voice".into())
                .spawn(move || actor::run(backend, cmd_rx, sh, sink));
            // A thread that will not start leaves the voice silent, not broken.
            match spawned {
                Ok(handle) => {
                    tx = Some(cmd_tx);
                    join = Some(handle);
                    idle_sink = None;
                }
                Err(_) => shared.available.store(false, Ordering::SeqCst),
            }
        }
        Voice {
            inner: Arc::new(Inner {
                home: home.to_path_buf(),
                shared,
                tx,
                join: Mutex::new(join),
                idle_sink: Mutex::new(idle_sink),
                next_id: AtomicU64::new(1),
                down: AtomicBool::new(false),
            }),
        }
    }

    /// Availability, settings, chosen voice and whether it is speaking (empty envelope).
    pub fn status(&self) -> VoiceStatus {
        let sh = &self.inner.shared;
        // Read apart from the literal: a guard there would live across `current()`.
        let enabled = lock(&sh.settings).enabled;
        VoiceStatus {
            available: sh.available.load(Ordering::SeqCst),
            enabled,
            voice: self.current().map(|v| v.display()),
            speaking: sh.speaking.load(Ordering::SeqCst),
            envelope: Vec::new(),
        }
    }

    pub fn voices(&self) -> Vec<VoiceInfo> {
        self.inner.shared.voices.clone()
    }

    /// The voice that would speak now (settings' choice via [`pick_voice`]).
    pub fn current(&self) -> Option<VoiceInfo> {
        let sh = &self.inner.shared;
        let preferred = lock(&sh.settings).voice.clone();
        pick_voice(&sh.voices, preferred.as_deref()).cloned()
    }

    pub fn settings(&self) -> VoiceSettings {
        lock(&self.inner.shared.settings).clone()
    }

    /// Changes and persists settings; `None` leaves a field as it is. An unknown voice name is
    /// refused and nothing changes; `""`, `auto` or `default` returns to automatic choice.
    /// Turning the voice off stops what it is saying.
    pub fn configure(&self, enabled: Option<bool>, voice: Option<String>) -> Result<VoiceStatus, VoiceError> {
        let sh = &self.inner.shared;
        {
            let mut cur = lock(&sh.settings);
            let mut next = cur.clone();
            if let Some(e) = enabled {
                next.enabled = e;
            }
            if let Some(name) = voice {
                next.voice = if is_auto(&name) {
                    None
                } else {
                    let v = select::find(&sh.voices, &name)
                        .ok_or_else(|| VoiceError::UnknownVoice(name.trim().to_string()))?;
                    Some(v.display())
                };
            }
            if next != *cur {
                std::fs::create_dir_all(&self.inner.home)?;
                next.save(&self.inner.home)?;
                *cur = next;
            }
        }
        if enabled == Some(false) {
            self.stop();
        }
        Ok(self.status())
    }

    /// Queues `text`; returns its utterance id. A new utterance does not interrupt one playing.
    pub fn speak(&self, text: &str) -> Result<u64, VoiceError> {
        let inner = &self.inner;
        if inner.down.load(Ordering::SeqCst) {
            return Err(VoiceError::Stopped);
        }
        let sh = &inner.shared;
        let Some(tx) = inner.tx.as_ref().filter(|_| sh.available.load(Ordering::SeqCst)) else {
            return Err(VoiceError::Unavailable);
        };
        if !lock(&sh.settings).enabled {
            return Err(VoiceError::Disabled);
        }
        let voice_id = self.current().map(|v| v.id).ok_or(VoiceError::Unavailable)?;
        let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let epoch = sh.epoch.load(Ordering::SeqCst);
        tx.send(Cmd::Speak { id, text: text.to_string(), voice_id, epoch }).map_err(|_| VoiceError::Stopped)?;
        Ok(id)
    }

    /// Barge-in: stops the current utterance and drops the queue.
    pub fn stop(&self) {
        self.inner.shared.epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(tx) = &self.inner.tx {
            let _ = tx.send(Cmd::Stop);
        }
    }

    /// Whether an utterance is being synthesized or played.
    pub fn is_speaking(&self) -> bool {
        self.inner.shared.speaking.load(Ordering::SeqCst)
    }

    /// Stops and joins the actor (idempotent). The event sink is closed once this returns.
    pub fn shutdown(&self) {
        let inner = &self.inner;
        if inner.down.swap(true, Ordering::SeqCst) {
            return;
        }
        inner.shared.epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(tx) = &inner.tx {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(handle) = lock(&inner.join).take() {
            let _ = handle.join();
        }
        lock(&inner.idle_sink).take();
    }
}

#[cfg(test)]
mod tests;
