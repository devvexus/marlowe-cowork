//! Spoken voice for the Wake (M3): WinRT `SpeechSynthesizer` on Windows, nothing elsewhere.
//!
//! The daemon owns one [`Voice`]: an actor thread that synthesizes text to WAV, computes an RMS
//! amplitude envelope at [`ENVELOPE_HZ`] for the presence meter, plays it, and reports
//! [`VoiceEvent`]s through a channel. [`Voice::stop`] is barge-in. Settings (on/off, voice name)
//! persist in `<home>\voice.json`. With no backend the handle still works and reports itself
//! unavailable; nothing is an error the caller must handle to keep going.
//!
//! `engine: "chatterbox"` in `voice.json` (or `MARLOWE_VOICE_ENGINE=chatterbox`) swaps synthesis
//! for a Chatterbox voice clone run as a Python sidecar ([`ChatterboxBackend`]), a placeholder:
//! WinRT still plays every utterance and speaks whenever the clone cannot (or is still loading).
//! The clone delivers an utterance clause by clause ([`Part`]), so long text starts at once. The
//! full voice (Kokoro, push-to-talk) replaces both in M6 as a sidecar.

#![forbid(unsafe_code)]

mod actor;
mod chatterbox;
mod select;
mod settings;
mod sidecar;
mod wav;
#[cfg(windows)]
mod winrt;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;

use actor::{Cmd, Shared};

pub use marlowe_proto::{VoiceStatus, ENVELOPE_HZ};
pub use chatterbox::{ChatterboxBackend, CLONE_ID};
pub use select::pick_voice;
pub use settings::{requested, ChatterboxSettings, Engine, Loaded, VoiceSettings, ENV_ENABLE};
pub use sidecar::{owns_process, VramCheck};
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
    /// The configured engine is not speaking and a fallback is (reported once per cause).
    Degraded { message: String },
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

/// One piece of an utterance: a WAV file, and whether it is the last piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub wav: Vec<u8>,
    pub last: bool,
}

/// A speech engine. Runs on the voice actor's thread.
pub trait Backend: Send {
    /// Installed voices.
    fn voices(&self) -> Vec<VoiceInfo>;
    /// Synthesizes `text` with voice `voice_id` to WAV bytes.
    fn synthesize(&mut self, text: &str, voice_id: &str) -> Result<Vec<u8>, VoiceError>;
    /// [`Backend::synthesize`] that may give up early once `cancelled()` is true (barge-in,
    /// shutdown). The default ignores it.
    fn synthesize_until(&mut self, text: &str, voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, VoiceError> {
        let _ = cancelled;
        self.synthesize(text, voice_id)
    }
    /// Streaming synthesis: the first part of `text` (blocking; may give up once `cancelled()`).
    /// When it is not `last`, the rest come from [`Backend::next_part`]. Default: all of
    /// [`Backend::synthesize_until`] as one part.
    fn begin_parts(&mut self, text: &str, voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Part, VoiceError> {
        self.synthesize_until(text, voice_id, cancelled).map(|wav| Part { wav, last: true })
    }
    /// The next part of the utterance [`Backend::begin_parts`] started, if it is ready within
    /// `wait` (`Ok(None)`: not yet). Called only after a part that was not `last`.
    fn next_part(&mut self, wait: Duration, cancelled: &dyn Fn() -> bool) -> Result<Option<Part>, VoiceError> {
        let _ = (wait, cancelled);
        Err(VoiceError::Backend("this engine speaks in one part".into()))
    }
    /// Drops the rest of a streamed utterance (barge-in, shutdown). Default: nothing.
    fn abandon(&mut self) {}
    /// [`Backend::begin_parts`] for speech nobody asked for (a notice): it must not load an
    /// engine, so an engine that is not loaded hands the line to one that is. Default: the same.
    fn begin_background_parts(&mut self, text: &str, voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Part, VoiceError> {
        self.begin_parts(text, voice_id, cancelled)
    }
    /// Gets ready to speak soon without blocking (a sidecar starts loading). Default: nothing.
    fn prewarm(&mut self) {}
    /// Epoch ms at which the engine began loading a model, while it still is (else 0): shared, so a
    /// client can show the load live. Default: an engine that loads nothing.
    fn loading_flag(&self) -> Option<Arc<AtomicU64>> {
        None
    }
    /// A degradation to report once (the engine fell back), taken by the actor.
    fn take_notice(&mut self) -> Option<String> {
        None
    }
    /// While the configured engine is not the one speaking: the voice that is, as clients should
    /// see it (`George (en-GB) - clone unavailable`). Default: never.
    fn degraded(&self) -> Option<String> {
        None
    }
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

/// The backend `settings` (with environment overrides already applied) ask for: WinRT, or the
/// Chatterbox clone over WinRT playback. `scrub` names environment variables a sidecar must not
/// inherit. `None` without WinRT (the clone needs its player).
pub fn engine_backend(home: &Path, settings: &VoiceSettings, scrub: &[&str]) -> Option<Box<dyn Backend>> {
    engine_backend_gated(home, settings, scrub, None)
}

/// [`engine_backend`] whose voice sidecar starts on the GPU only when `vram` says it fits.
pub fn engine_backend_gated(home: &Path, settings: &VoiceSettings, scrub: &[&str], vram: Option<VramCheck>) -> Option<Box<dyn Backend>> {
    let player = default_backend()?;
    match settings.engine {
        Engine::Winrt => Some(player),
        Engine::Chatterbox => {
            let cb = settings.chatterbox.clone().unwrap_or_default();
            let mut cfg = sidecar::SidecarConfig::from_settings(home, &cb, scrub);
            cfg.vram = vram;
            eprintln!(
                "marlowe: voice: Chatterbox clone (placeholder) with {} cloning {}; WinRT plays and stands in",
                cb.python.as_deref().map_or("no python set".into(), |p| p.display().to_string()),
                cb.reference.as_deref().map_or("no sample set".into(), |p| p.display().to_string()),
            );
            Some(Box::new(ChatterboxBackend::new(player, cfg, settings.voice.clone())))
        }
    }
}

/// A static check of the configured engine for `marlowe doctor`: `Ok(description)` when it looks
/// runnable, `Err(what is missing)` otherwise. Starts nothing.
pub fn engine_check(settings: &VoiceSettings) -> Result<String, String> {
    match settings.engine {
        Engine::Winrt => Ok("WinRT (Windows voices)".into()),
        Engine::Chatterbox => {
            let cb = settings.chatterbox.clone().unwrap_or_default();
            let python = cb.python.ok_or("Chatterbox: no python set (chatterbox.python or MARLOWE_VOICE_PYTHON)")?;
            let reference = cb.reference.ok_or("Chatterbox: no voice sample set (chatterbox.reference or MARLOWE_VOICE_REF)")?;
            // A bare name (`python`) is looked up on PATH at spawn; only a path can be checked here.
            if python.components().count() > 1 && !python.is_file() {
                return Err(format!("Chatterbox: {} does not exist", python.display()));
            }
            if !reference.is_file() {
                return Err(format!("Chatterbox: the sample {} does not exist", reference.display()));
            }
            if let Some(script) = cb.script.as_deref().filter(|s| !s.is_file()) {
                return Err(format!("Chatterbox: the script {} does not exist", script.display()));
            }
            Ok(format!(
                "Chatterbox clone (placeholder) of {} via {}, device {}; WinRT fallback",
                reference.display(),
                python.display(),
                cb.device
            ))
        }
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
    /// Starts the actor with the engine `<home>\voice.json` (and the `MARLOWE_VOICE_*`
    /// environment) choose. Never fails: without a backend the voice reports itself unavailable.
    pub fn start(home: &Path, sink: Sender<VoiceEvent>) -> Voice {
        Self::start_with(home, sink, &[])
    }

    /// [`Voice::start`]; `scrub` names environment variables (API keys) a voice sidecar must not
    /// inherit.
    pub fn start_with(home: &Path, sink: Sender<VoiceEvent>, scrub: &[&str]) -> Voice {
        Self::start_gated(home, sink, scrub, None)
    }

    /// [`Voice::start_with`]; a voice sidecar loads onto the GPU only when `vram` says its
    /// footprint fits beside what is there (otherwise the Windows voice speaks).
    pub fn start_gated(home: &Path, sink: Sender<VoiceEvent>, scrub: &[&str], vram: Option<VramCheck>) -> Voice {
        let settings = VoiceSettings::load(home).with_env(&|k| std::env::var(k).ok());
        Self::with_backend(home, sink, engine_backend_gated(home, &settings, scrub, vram))
    }

    /// [`Voice::start`] with an explicit backend (tests pass a fake, or `None`).
    pub fn with_backend(home: &Path, sink: Sender<VoiceEvent>, backend: Option<Box<dyn Backend>>) -> Voice {
        let voices = backend.as_ref().map(|b| b.voices()).unwrap_or_default();
        // A backend with no installed voices cannot speak.
        let backend = backend.filter(|_| !voices.is_empty());
        let loaded = VoiceSettings::read(home);
        let mut sh = Shared::new(backend.is_some(), voices, loaded.settings);
        sh.load = backend.as_ref().and_then(|b| b.loading_flag());
        let shared = Arc::new(sh);
        if !loaded.problems.is_empty() {
            *lock(&shared.unsaved) = Some(loaded.problems.join("; "));
        }
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

    /// Availability, settings, the voice speaking (the fallback's, while the configured engine
    /// cannot) and whether it is speaking (empty envelope).
    pub fn status(&self) -> VoiceStatus {
        let sh = &self.inner.shared;
        // Read apart from the literal: a guard there would live across `current()`.
        let enabled = lock(&sh.settings).enabled;
        let degraded = lock(&sh.degraded).clone();
        VoiceStatus {
            available: sh.available.load(Ordering::SeqCst),
            enabled,
            voice: degraded.or_else(|| self.current().map(|v| v.display())),
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
    /// Turning the voice off stops what it is saying. A `voice.json` with fields that could not be
    /// read is not overwritten: the change then lasts until the daemon restarts.
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
                match lock(&sh.unsaved).as_deref() {
                    Some(why) => eprintln!(
                        "marlowe: voice: not saving {} ({why}); fix it to keep this change",
                        VoiceSettings::path(&self.inner.home).display()
                    ),
                    None => {
                        std::fs::create_dir_all(&self.inner.home)?;
                        next.save(&self.inner.home)?;
                    }
                }
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
        self.queue(text, false)
    }

    /// [`Voice::speak`] for a line nobody asked for (a notice): it never loads a voice model. A
    /// clone that is not loaded leaves the line to the Windows voice.
    pub fn speak_background(&self, text: &str) -> Result<u64, VoiceError> {
        self.queue(text, true)
    }

    fn queue(&self, text: &str, background: bool) -> Result<u64, VoiceError> {
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
        tx.send(Cmd::Speak { id, text: text.to_string(), voice_id, epoch, background }).map_err(|_| VoiceError::Stopped)?;
        Ok(id)
    }

    /// Epoch ms at which the engine's model began loading, while it is still loading: a cost the
    /// daemon shows live. `None` when nothing is loading.
    pub fn loading_since_ms(&self) -> Option<i64> {
        let at = self.inner.shared.load.as_ref()?.load(Ordering::SeqCst);
        (at > 0).then_some(at as i64)
    }

    /// Lets the engine get ready to speak (a voice sidecar starts loading its model) when the
    /// voice is on. Never blocks.
    pub fn prewarm(&self) {
        let inner = &self.inner;
        let sh = &inner.shared;
        if inner.down.load(Ordering::SeqCst) || !sh.available.load(Ordering::SeqCst) || !lock(&sh.settings).enabled {
            return;
        }
        if let Some(tx) = &inner.tx {
            let _ = tx.send(Cmd::Prewarm);
        }
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
