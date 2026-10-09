//! WAV parsing and the RMS amplitude envelope that drives the presence meter.

use crate::VoiceError;

/// Decoded PCM, interleaved, normalised to -1.0..=1.0.
#[derive(Debug, Clone, PartialEq)]
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl Wav {
    /// Length in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        let frames = self.samples.len() as u64 / u64::from(self.channels.max(1));
        frames * 1000 / u64::from(self.sample_rate.max(1))
    }
}

const FORMAT_PCM: u16 = 1;
const FORMAT_FLOAT: u16 = 3;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

fn bad(msg: impl Into<String>) -> VoiceError {
    VoiceError::BadWav(msg.into())
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

struct Format {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    bits: u16,
}

/// Parses a RIFF/WAVE file: PCM 8/16/24/32-bit or IEEE float 32, any channel count; skips
/// unknown chunks.
pub fn parse_wav(bytes: &[u8]) -> Result<Wav, VoiceError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let mut fmt: Option<Format> = None;
    let mut data: Option<&[u8]> = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32_at(bytes, at + 4) as usize;
        let body_start = at + 8;
        // Streaming writers leave the size as 0 or 0xFFFFFFFF; clamp to what is there.
        let body_end = body_start.saturating_add(len).min(bytes.len());
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(bad("fmt chunk too short"));
                }
                let mut tag = u16_at(body, 0);
                if tag == FORMAT_EXTENSIBLE {
                    // The sub-format GUID's first two bytes carry the real tag.
                    if body.len() < 26 {
                        return Err(bad("extensible fmt chunk too short"));
                    }
                    tag = u16_at(body, 24);
                }
                fmt = Some(Format { tag, channels: u16_at(body, 2), sample_rate: u32_at(body, 4), bits: u16_at(body, 14) });
            }
            b"data" => {
                data = Some(if len == 0 || len == u32::MAX as usize { &bytes[body_start..] } else { body });
            }
            _ => {}
        }
        if data.is_some() && fmt.is_some() {
            break;
        }
        // Chunks are word-aligned.
        at = body_start.saturating_add(len).saturating_add(len & 1);
    }
    let fmt = fmt.ok_or_else(|| bad("no fmt chunk"))?;
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    if fmt.channels == 0 || fmt.sample_rate == 0 {
        return Err(bad("zero channels or sample rate"));
    }
    let samples = decode(&fmt, data)?;
    Ok(Wav { sample_rate: fmt.sample_rate, channels: fmt.channels, samples })
}

fn decode(fmt: &Format, data: &[u8]) -> Result<Vec<f32>, VoiceError> {
    let width = usize::from(fmt.bits / 8);
    if width == 0 || !fmt.bits.is_multiple_of(8) {
        return Err(bad(format!("unsupported bit depth {}", fmt.bits)));
    }
    let chunks = data.chunks_exact(width);
    let out = match (fmt.tag, fmt.bits) {
        (FORMAT_PCM, 8) => chunks.map(|c| (f32::from(c[0]) - 128.0) / 128.0).collect(),
        (FORMAT_PCM, 16) => chunks.map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0).collect(),
        (FORMAT_PCM, 24) => chunks
            .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0)
            .collect(),
        (FORMAT_PCM, 32) => chunks
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        (FORMAT_FLOAT, 32) => chunks
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .map(|s| if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 })
            .collect(),
        (tag, bits) => return Err(bad(format!("unsupported format tag {tag} at {bits} bits"))),
    };
    Ok(out)
}

/// RMS amplitude per `1/hz` s window over all channels, scaled to 0..=255 (full-scale sine ~180).
/// A trailing partial window counts as a frame.
pub fn envelope(wav: &Wav, hz: u32) -> Vec<u8> {
    let channels = usize::from(wav.channels.max(1));
    let hz = hz.max(1);
    let window_frames = ((wav.sample_rate / hz) as usize).max(1);
    let window = window_frames * channels;
    wav.samples
        .chunks(window)
        .map(|w| {
            let mean_sq = w.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / w.len() as f64;
            (mean_sq.sqrt() * 255.0).round().clamp(0.0, 255.0) as u8
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a WAV file from `fmt` fields and raw data, with an extra chunk before `data`.
    pub(crate) fn build(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        let block = channels * bits / 8;
        fmt.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        fmt.extend_from_slice(&block.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        body.extend_from_slice(&fmt);
        // An odd-length LIST chunk exercises skipping and word padding.
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(b"abc\0");
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// 16-bit mono: `secs` of a 440 Hz sine at `amp`, then `silence` s of zeros.
    pub(crate) fn sine16(rate: u32, secs: f32, amp: f32, silence: f32) -> Vec<u8> {
        let n = (rate as f32 * secs) as usize;
        let z = (rate as f32 * silence) as usize;
        let mut data = Vec::with_capacity((n + z) * 2);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin() * amp;
            data.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
        }
        data.resize(data.len() + z * 2, 0);
        build(FORMAT_PCM, 1, rate, 16, &data)
    }

    #[test]
    fn sine_then_silence_gives_flat_rms_then_zero() {
        let wav = parse_wav(&sine16(24_000, 1.0, 0.5, 0.5)).unwrap();
        assert_eq!((wav.sample_rate, wav.channels, wav.samples.len()), (24_000, 1, 36_000));
        assert_eq!(wav.duration_ms(), 1500);
        let env = envelope(&wav, 30);
        assert_eq!(env.len(), 45);
        // 0.5 * 0.7071 * 255 = 90.
        for v in &env[..30] {
            assert!((89..=91).contains(v), "sine frame {v}");
        }
        assert!(env[30..].iter().all(|v| *v == 0));
    }

    #[test]
    fn full_scale_sine_is_about_180() {
        // 24 kHz / 30 Hz = 800-frame windows; 0.5 s is exactly 15 of them.
        let wav = parse_wav(&sine16(24_000, 0.5, 1.0, 0.0)).unwrap();
        let env = envelope(&wav, 30);
        assert!(env.iter().all(|v| (178..=182).contains(v)), "{env:?}");
    }

    #[test]
    fn trailing_partial_window_counts() {
        // 1000 samples at 3000 Hz, 30 Hz windows of 100 frames -> 10; 1050 -> 11.
        let wav = Wav { sample_rate: 3000, channels: 1, samples: vec![0.25; 1050] };
        let env = envelope(&wav, 30);
        assert_eq!(env.len(), 11);
        assert_eq!(env[10], 64);
        assert!(envelope(&Wav { sample_rate: 3000, channels: 1, samples: vec![] }, 30).is_empty());
    }

    #[test]
    fn decodes_8_24_32_float_and_stereo() {
        let w8 = parse_wav(&build(FORMAT_PCM, 1, 8000, 8, &[128, 255, 0])).unwrap();
        assert_eq!(w8.samples, vec![0.0, 127.0 / 128.0, -1.0]);

        let w24 = parse_wav(&build(FORMAT_PCM, 1, 8000, 24, &[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0])).unwrap();
        assert_eq!(w24.samples, vec![0.5, -0.5]);

        let w32 = parse_wav(&build(FORMAT_PCM, 1, 8000, 32, &(i32::MIN / 2).to_le_bytes())).unwrap();
        assert_eq!(w32.samples, vec![-0.5]);

        let mut f = Vec::new();
        for s in [0.25f32, -2.0, f32::NAN] {
            f.extend_from_slice(&s.to_le_bytes());
        }
        let wf = parse_wav(&build(FORMAT_FLOAT, 1, 8000, 32, &f)).unwrap();
        assert_eq!(wf.samples, vec![0.25, -1.0, 0.0]);

        // Stereo: windows span both channels; 2 frames per channel pair.
        let mut st = Vec::new();
        for s in [16384i16, -16384, 16384, -16384] {
            st.extend_from_slice(&s.to_le_bytes());
        }
        let ws = parse_wav(&build(FORMAT_PCM, 2, 4, 16, &st)).unwrap();
        assert_eq!(ws.channels, 2);
        assert_eq!(ws.duration_ms(), 500);
        assert_eq!(envelope(&ws, 2), vec![128]);
    }

    #[test]
    fn extensible_format_uses_the_subformat_tag() {
        let mut bytes = build(FORMAT_EXTENSIBLE, 1, 8000, 16, &[0, 0x40]);
        // Grow the fmt chunk to 40 bytes with a PCM sub-format.
        let fmt_at = 12;
        let mut fmt = bytes[fmt_at + 8..fmt_at + 24].to_vec();
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        fmt.extend_from_slice(&0u32.to_le_bytes());
        fmt.extend_from_slice(&FORMAT_PCM.to_le_bytes());
        fmt.extend_from_slice(&[0u8; 14]);
        let mut out = bytes[..fmt_at + 4].to_vec();
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(&bytes.split_off(fmt_at + 24));
        let wav = parse_wav(&out).unwrap();
        assert_eq!(wav.samples, vec![0.5]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(parse_wav(b"nope"), Err(VoiceError::BadWav(_))));
        assert!(matches!(parse_wav(b"RIFF\0\0\0\0WAVE"), Err(VoiceError::BadWav(_))));
        assert!(parse_wav(&build(FORMAT_PCM, 1, 8000, 12, &[0, 0])).is_err());
        assert!(parse_wav(&build(7, 1, 8000, 8, &[0])).is_err());
    }

    #[test]
    fn streaming_data_size_reads_to_the_end() {
        let mut bytes = build(FORMAT_PCM, 1, 8000, 16, &[0, 0x40, 0, 0xC0]);
        let n = bytes.len();
        bytes[n - 8..n - 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_wav(&bytes).unwrap().samples, vec![0.5, -0.5]);
    }
}
