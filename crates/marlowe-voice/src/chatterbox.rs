//! The Chatterbox voice clone: a placeholder engine until the M6 voice. Synthesis goes through the
//! sidecar ([`crate::sidecar`]), clause by clause; playback, stop, `is_playing` and the playback
//! position are the player's (WinRT in production), so barge-in and the presence envelope behave
//! as with WinRT.
//!
//! The player's own synthesizer says a line instead when the clone cannot: while the model is
//! still loading (after [`crate::ChatterboxSettings::ready_grace_s`]; not a degradation, the load
//! carries on), and when the sidecar fails (reported once per kind of failure through
//! [`Backend::take_notice`], and shown by [`Backend::degraded`]). A sidecar that would not start,
//! died, hung, ran out of GPU memory, or refused [`MAX_REFUSALS`] lines in a row is dropped, which
//! frees its VRAM, and left alone for [`RETRY_AFTER`].
//!
//! The clone never loads for a line nobody asked for ([`Backend::begin_background_parts`]: a
//! notice), and a GPU start waits for its footprint to fit (`SidecarConfig::vram`).

use std::time::{Duration, Instant};

use crate::sidecar::{Sidecar, SidecarConfig, SidecarError};
use crate::{pick_voice, Backend, Part, VoiceError, VoiceGender, VoiceInfo};

/// The one voice this engine offers.
pub const CLONE_ID: &str = "chatterbox:clone";
/// How long a dropped sidecar is left alone before the next attempt.
pub const RETRY_AFTER: Duration = Duration::from_secs(600);
/// Refused lines in a row after which the sidecar is taken to be broken.
pub const MAX_REFUSALS: u32 = 2;

fn clone_voice() -> VoiceInfo {
    VoiceInfo { id: CLONE_ID.into(), name: "Chatterbox clone".into(), language: "en".into(), gender: VoiceGender::Unknown }
}

/// Errors after which CUDA (and so the sidecar) does not recover: out of memory, or a CUDA error
/// that poisons the context.
fn is_sticky(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    ["outofmemoryerror", "out of memory", "cuda error", "cublas_status", "cudnn_status"].iter().any(|k| m.contains(k))
}

/// Chatterbox synthesis with a WinRT (or any) player and fallback.
pub struct ChatterboxBackend {
    player: Box<dyn Backend>,
    sidecar: Sidecar,
    /// The player's voices and the preferred one, for fallback synthesis.
    fallback_voices: Vec<VoiceInfo>,
    fallback_pref: Option<String>,
    /// Set when the sidecar was dropped: fallback until [`RETRY_AFTER`] has passed.
    cooling: Option<Instant>,
    /// The kind of failure last reported ([`SidecarError::cause`]); cleared when the clone speaks.
    reported: Option<String>,
    notice: Option<String>,
    /// Lines refused in a row.
    refusals: u32,
    /// The utterance being streamed, for the fallback when the sidecar fails before saying how
    /// it split it.
    text: String,
}

impl ChatterboxBackend {
    pub(crate) fn new(player: Box<dyn Backend>, cfg: SidecarConfig, fallback_pref: Option<String>) -> ChatterboxBackend {
        let fallback_voices = player.voices();
        ChatterboxBackend {
            player,
            sidecar: Sidecar::new(cfg),
            fallback_voices,
            fallback_pref,
            cooling: None,
            reported: None,
            notice: None,
            refusals: 0,
            text: String::new(),
        }
    }

    fn cooling(&self) -> bool {
        self.cooling.is_some_and(|t| t.elapsed() < RETRY_AFTER)
    }

    fn recovered(&mut self) {
        if self.reported.take().is_some() {
            eprintln!("marlowe: voice: the Chatterbox voice is back");
        }
        self.cooling = None;
        self.refusals = 0;
    }

    /// A sidecar failure: a broken sidecar is dropped and cooled; one refused line is only
    /// reported. Every failure is logged; clients hear of each kind once.
    fn failed(&mut self, err: &SidecarError) {
        let broken = match err {
            SidecarError::Synth(m) | SidecarError::Protocol(m) => {
                self.refusals += 1;
                self.refusals >= MAX_REFUSALS || is_sticky(m)
            }
            _ => true,
        };
        if broken {
            self.sidecar.reset();
            self.cooling = Some(Instant::now());
        } else {
            self.sidecar.abandon();
        }
        let msg = format!("the Chatterbox voice is unavailable ({err}); speaking with the Windows voice");
        let stopped = if broken { format!("; the sidecar is stopped for {} min", RETRY_AFTER.as_secs() / 60) } else { String::new() };
        eprintln!("marlowe: voice: {msg}{stopped}");
        let cause = err.cause();
        if self.reported.as_deref() != Some(cause.as_str()) {
            self.notice = Some(msg);
            self.reported = Some(cause);
        }
    }

    fn fallback(&mut self, text: &str, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, VoiceError> {
        let id = pick_voice(&self.fallback_voices, self.fallback_pref.as_deref())
            .map(|v| v.id.clone())
            .ok_or(VoiceError::Unavailable)?;
        self.player.synthesize_until(text, &id, cancelled)
    }

    fn fallback_part(&mut self, text: &str, cancelled: &dyn Fn() -> bool) -> Result<Part, VoiceError> {
        self.fallback(text, cancelled).map(|wav| Part { wav, last: true })
    }
}

impl Backend for ChatterboxBackend {
    fn voices(&self) -> Vec<VoiceInfo> {
        vec![clone_voice()]
    }

    fn synthesize(&mut self, text: &str, voice_id: &str) -> Result<Vec<u8>, VoiceError> {
        self.synthesize_until(text, voice_id, &|| false)
    }

    /// All of `text` as one WAV (rendering to a buffer), waiting out a cold load.
    fn synthesize_until(&mut self, text: &str, _voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, VoiceError> {
        if self.cooling() {
            return self.fallback(text, cancelled);
        }
        match self.sidecar.synthesize(text, cancelled) {
            Ok(bytes) => {
                self.recovered();
                Ok(bytes)
            }
            Err(SidecarError::Cancelled) => Err(VoiceError::Stopped),
            Err(e) => {
                self.failed(&e);
                self.fallback(text, cancelled)
            }
        }
    }

    fn begin_parts(&mut self, text: &str, _voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Part, VoiceError> {
        self.text.clear();
        if self.cooling() {
            return self.fallback_part(text, cancelled);
        }
        let grace = self.sidecar.grace();
        match self.sidecar.first_part(text, false, grace, cancelled) {
            Ok(Some(part)) => {
                self.recovered();
                if !part.last {
                    self.text = text.to_string();
                }
                Ok(part)
            }
            Ok(None) => {
                eprintln!("marlowe: voice: the Chatterbox model is still loading; the Windows voice says this line");
                self.fallback_part(text, cancelled)
            }
            Err(SidecarError::Cancelled) => Err(VoiceError::Stopped),
            Err(e) => {
                self.failed(&e);
                self.fallback_part(text, cancelled)
            }
        }
    }

    /// A notice never loads the clone: the clone says it only when its model is already up;
    /// otherwise the Windows voice does, and nothing is reported (nothing failed).
    fn begin_background_parts(&mut self, text: &str, voice_id: &str, cancelled: &dyn Fn() -> bool) -> Result<Part, VoiceError> {
        if !self.cooling() && self.sidecar.is_loaded() {
            return self.begin_parts(text, voice_id, cancelled);
        }
        self.text.clear();
        self.fallback_part(text, cancelled)
    }

    fn next_part(&mut self, wait: Duration, cancelled: &dyn Fn() -> bool) -> Result<Option<Part>, VoiceError> {
        match self.sidecar.next_part(wait, cancelled) {
            Ok(Some(part)) => {
                if part.last {
                    self.text.clear();
                }
                Ok(Some(part))
            }
            Ok(None) => Ok(None),
            Err(SidecarError::Cancelled) => Err(VoiceError::Stopped),
            Err(e) => {
                // Mid-utterance: the Windows voice says what the clone had not.
                let rest = self.sidecar.take_rest().unwrap_or_else(|| std::mem::take(&mut self.text));
                self.text.clear();
                self.failed(&e);
                if rest.trim().is_empty() {
                    return Err(VoiceError::Backend(e.to_string()));
                }
                self.fallback_part(&rest, cancelled).map(Some)
            }
        }
    }

    fn abandon(&mut self) {
        self.sidecar.abandon();
        self.text.clear();
    }

    fn prewarm(&mut self) {
        if self.cooling() {
            return;
        }
        if let Err(e) = self.sidecar.start() {
            self.failed(&e);
        }
    }

    fn loading_flag(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicU64>> {
        Some(self.sidecar.loading_flag())
    }

    fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn degraded(&self) -> Option<String> {
        if self.reported.is_none() && !self.cooling() {
            return None;
        }
        let v = pick_voice(&self.fallback_voices, self.fallback_pref.as_deref())?;
        Some(format!("{} - clone unavailable", v.display()))
    }

    fn play(&mut self, wav: Vec<u8>) -> Result<(), VoiceError> {
        self.player.play(wav)
    }

    fn stop(&mut self) {
        self.player.stop();
    }

    fn is_playing(&self) -> bool {
        self.player.is_playing()
    }

    fn position_ms(&self) -> Option<u64> {
        self.player.position_ms()
    }
}
