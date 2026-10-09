//! WinRT `SpeechSynthesizer` + `MediaPlayer`. Windows only.
//!
//! Synthesis goes to an in-memory WAV (`SynthesizeTextToStreamAsync`, read through a
//! `DataReader`), so it is testable without playing anything. Playback wraps the bytes in an
//! `InMemoryRandomAccessStream` and hands them to a fresh `MediaPlayer` (speech category, kept out
//! of the system media controls). Blocking waits use `IAsyncOperation::join` on the actor thread
//! (implicit MTA).

use windows::core::HSTRING;
use windows::Media::Core::MediaSource;
use windows::Media::Playback::{MediaPlaybackState, MediaPlayer, MediaPlayerAudioCategory};
use windows::Media::SpeechSynthesis::{SpeechSynthesizer, VoiceGender as WinGender};
use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};

use crate::{Backend, VoiceError, VoiceGender, VoiceInfo};

/// Synthesized audio larger than this is refused (about 25 minutes of 22 kHz mono speech).
const MAX_WAV: u64 = 64 * 1024 * 1024;

/// The WinRT speech backend.
pub struct WinRtBackend {
    synth: SpeechSynthesizer,
    /// The voice id last set on `synth`.
    voice_id: Option<String>,
    player: Option<MediaPlayer>,
}

fn backend_err(e: windows::core::Error) -> VoiceError {
    let msg = e.message().to_string();
    VoiceError::Backend(if msg.is_empty() { format!("{:?}", e.code()) } else { msg })
}

impl WinRtBackend {
    /// Creates the synthesizer; fails when the speech runtime is missing.
    pub fn new() -> Result<WinRtBackend, VoiceError> {
        Ok(WinRtBackend { synth: SpeechSynthesizer::new().map_err(backend_err)?, voice_id: None, player: None })
    }

    /// Every installed voice.
    pub fn installed() -> Result<Vec<VoiceInfo>, VoiceError> {
        let all = SpeechSynthesizer::AllVoices().map_err(backend_err)?;
        let mut out = Vec::new();
        for i in 0..all.Size().map_err(backend_err)? {
            let v = all.GetAt(i).map_err(backend_err)?;
            let display = v.DisplayName().map_err(backend_err)?.to_string();
            out.push(VoiceInfo {
                id: v.Id().map_err(backend_err)?.to_string(),
                name: short_name(&display),
                language: v.Language().map_err(backend_err)?.to_string(),
                gender: match v.Gender().map_err(backend_err)? {
                    WinGender::Male => VoiceGender::Male,
                    WinGender::Female => VoiceGender::Female,
                    _ => VoiceGender::Unknown,
                },
            });
        }
        // The system default first: it is pick_voice's last resort.
        if let Ok(default) = SpeechSynthesizer::DefaultVoice().and_then(|v| v.Id()).map(|h| h.to_string()) {
            if let Some(i) = out.iter().position(|v| v.id == default) {
                let d = out.remove(i);
                out.insert(0, d);
            }
        }
        Ok(out)
    }

    /// Sets the synthesizer's voice to `voice_id` (an id from [`WinRtBackend::installed`]).
    fn select(&mut self, voice_id: &str) -> Result<(), VoiceError> {
        if voice_id.is_empty() || self.voice_id.as_deref() == Some(voice_id) {
            return Ok(());
        }
        let all = SpeechSynthesizer::AllVoices().map_err(backend_err)?;
        for i in 0..all.Size().map_err(backend_err)? {
            let v = all.GetAt(i).map_err(backend_err)?;
            if v.Id().map_err(backend_err)? == voice_id {
                self.synth.SetVoice(&v).map_err(backend_err)?;
                self.voice_id = Some(voice_id.to_string());
                return Ok(());
            }
        }
        Err(VoiceError::UnknownVoice(voice_id.to_string()))
    }

    fn close_player(&mut self) {
        if let Some(p) = self.player.take() {
            let _ = p.Pause();
            let _ = p.Close();
        }
    }
}

impl Drop for WinRtBackend {
    fn drop(&mut self) {
        self.close_player();
    }
}

/// `Microsoft George Desktop` -> `George`.
fn short_name(display: &str) -> String {
    let s = display.trim();
    let s = s.strip_prefix("Microsoft ").unwrap_or(s);
    let s = s.strip_suffix(" Desktop").unwrap_or(s);
    s.split(" - ").next().unwrap_or(s).trim().to_string()
}

impl Backend for WinRtBackend {
    fn voices(&self) -> Vec<VoiceInfo> {
        Self::installed().unwrap_or_default()
    }

    fn synthesize(&mut self, text: &str, voice_id: &str) -> Result<Vec<u8>, VoiceError> {
        self.select(voice_id)?;
        let stream = self.synth.SynthesizeTextToStreamAsync(&HSTRING::from(text)).map_err(backend_err)?.join().map_err(backend_err)?;
        let size = stream.Size().map_err(backend_err)?;
        if size > MAX_WAV {
            return Err(VoiceError::Backend(format!("synthesized audio too large ({size} bytes)")));
        }
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).map_err(backend_err)?).map_err(backend_err)?;
        let loaded = reader.LoadAsync(size as u32).map_err(backend_err)?.join().map_err(backend_err)?;
        let mut bytes = vec![0u8; loaded as usize];
        reader.ReadBytes(&mut bytes).map_err(backend_err)?;
        let _ = stream.Close();
        Ok(bytes)
    }

    fn play(&mut self, wav: Vec<u8>) -> Result<(), VoiceError> {
        self.close_player();
        let stream = InMemoryRandomAccessStream::new().map_err(backend_err)?;
        let writer = DataWriter::CreateDataWriter(&stream).map_err(backend_err)?;
        writer.WriteBytes(&wav).map_err(backend_err)?;
        writer.StoreAsync().map_err(backend_err)?.join().map_err(backend_err)?;
        writer.DetachStream().map_err(backend_err)?;
        stream.Seek(0).map_err(backend_err)?;
        let source = MediaSource::CreateFromStream(&stream, &HSTRING::from("audio/wav")).map_err(backend_err)?;
        let player = MediaPlayer::new().map_err(backend_err)?;
        let _ = player.SetAudioCategory(MediaPlayerAudioCategory::Speech);
        // Keep Marlowe's voice out of the system media overlay.
        if let Ok(cm) = player.CommandManager() {
            let _ = cm.SetIsEnabled(false);
        }
        player.SetSource(&source).map_err(backend_err)?;
        player.Play().map_err(backend_err)?;
        self.player = Some(player);
        Ok(())
    }

    fn stop(&mut self) {
        self.close_player();
    }

    fn is_playing(&self) -> bool {
        let Some(p) = &self.player else { return false };
        match p.PlaybackSession().and_then(|s| s.PlaybackState()) {
            Ok(s) => s == MediaPlaybackState::Opening || s == MediaPlaybackState::Buffering || s == MediaPlaybackState::Playing,
            Err(_) => false,
        }
    }

    fn position_ms(&self) -> Option<u64> {
        let p = self.player.as_ref()?;
        let ticks = p.PlaybackSession().and_then(|s| s.Position()).ok()?.Duration;
        // TimeSpan is in 100 ns units.
        Some(u64::try_from(ticks / 10_000).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pick_voice, render, ENVELOPE_HZ};

    #[test]
    fn display_names_shorten() {
        assert_eq!(short_name("Microsoft George Desktop"), "George");
        assert_eq!(short_name("Microsoft Hazel - English (Great Britain)"), "Hazel");
        assert_eq!(short_name("Zira"), "Zira");
    }

    /// Lists real installed voices (touches the WinRT speech runtime; no audio).
    #[test]
    #[ignore]
    fn lists_installed_voices() {
        let voices = WinRtBackend::installed().unwrap();
        for v in &voices {
            println!("{} [{:?}] {}", v.display(), v.gender, v.id);
        }
        assert!(!voices.is_empty());
        println!("chosen: {}", pick_voice(&voices, None).unwrap().display());
    }

    /// Synthesizes to a buffer with the real engine and checks the PCM. Plays nothing.
    #[test]
    #[ignore]
    fn synthesizes_good_morning_to_a_buffer() {
        let mut backend = WinRtBackend::new().unwrap();
        let r = render(&mut backend, "Good morning, sir.", None).unwrap();
        println!(
            "voice: {} [{:?}]; {} bytes, {} Hz x{}, {} ms; envelope {} frames, peak {}",
            r.voice.display(),
            r.voice.gender,
            r.bytes.len(),
            r.wav.sample_rate,
            r.wav.channels,
            r.wav.duration_ms(),
            r.envelope.len(),
            r.envelope.iter().max().copied().unwrap_or(0),
        );
        assert!(!r.wav.samples.is_empty());
        assert!(r.wav.samples.iter().any(|s| s.abs() > 0.01), "synthesized PCM is silent");
        assert!(r.wav.duration_ms() > 300);
        let expected = r.wav.duration_ms() * u64::from(ENVELOPE_HZ) / 1000;
        assert!((r.envelope.len() as u64).abs_diff(expected) <= 1, "{} frames for {expected}", r.envelope.len());
        assert!(r.envelope.iter().any(|v| *v > 0));
    }

    /// Plays a short line through the speakers, then barges in. Plays audio: run by hand.
    #[test]
    #[ignore]
    fn plays_and_stops_real_audio() {
        let mut backend = WinRtBackend::new().unwrap();
        let r = render(&mut backend, "Good morning, sir. This sentence is cut short.", None).unwrap();
        backend.play(r.bytes).unwrap();
        let start = std::time::Instant::now();
        while !backend.is_playing() && start.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(backend.is_playing());
        std::thread::sleep(std::time::Duration::from_millis(900));
        assert!(backend.position_ms().unwrap_or(0) > 0);
        backend.stop();
        assert!(!backend.is_playing());
    }
}
