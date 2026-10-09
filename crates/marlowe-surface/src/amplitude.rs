//! The **scripted** sample source behind ADR-021's meter.
//!
//! **This is a synthetic envelope, not a microphone.** Stated in ADR-021 and repeated here because
//! this is the file where it would rot. §B12 forbids decorative motion; what satisfies it is that
//! the *widget* invents nothing and renders what its source reports. That stays structurally true
//! and this reading stays synthetic.
//!
//! # C2d moved the shape out and left the generator here, deliberately
//!
//! `Frame`, `SAMPLES`, `LEVELS` and `BASELINE` are now `marlowe_view::meter` — a surface has to
//! know the shape of a reading in order to draw one. **`sample` did not go with them.** A surface
//! that could call it could generate its own amplitude, which is precisely the state ARCHITECTURE
//! §2.14 says it must not hold. Producing a reading is a producer's job; `marlowe-daemon` has its
//! own, measured from real delta arrivals rather than from a wave.

use marlowe_view::meter::{Frame, BASELINE, LEVELS, SAMPLES};
use marlowe_view::StatusState;

/// Produce the frame for a state at a time, or `None` when the state does not sample.
///
/// **`None` is the entire freeze mechanism.** `waiting` returns it, the caller holds the previous
/// frame, and the indicator stops moving because nothing is feeding it — not because a branch
/// somewhere disabled an animation. See ADR-021 and [`StatusState::samples_amplitude`].
pub fn sample(state: StatusState, now_ms: u64, run_elapsed_ms: u64, run_expected_ms: u64) -> Option<Frame> {
    if !state.samples_amplitude() {
        return None;
    }
    Some(match state {
        // Voice: an amplitude envelope with a speech-like cadence — syllable-rate bursts rather
        // than a sine, because a sine reads as decoration the moment you look at it twice.
        StatusState::Listening => envelope(now_ms, 380, 6, 2),
        StatusState::Speaking => envelope(now_ms, 300, 7, 3),
        // Stream progress: a busier, lower-amplitude texture. Thinking is not silent, but it is
        // not speech either.
        StatusState::Thinking => envelope(now_ms, 210, 5, 1),
        StatusState::Writing => envelope(now_ms, 520, 4, 1),
        // §B5: in `running` the indicator tracks elapsed against the expected duration. This is
        // real progress, not an oscillation, so it fills left to right and does not come back.
        StatusState::Running => progress(run_elapsed_ms, run_expected_ms),
        StatusState::Idle => BASELINE,
        StatusState::Waiting => unreachable!("guarded by samples_amplitude above"),
    })
}

/// A deterministic pseudo-amplitude envelope.
///
/// Deterministic on purpose: `(state, now_ms)` fully determines the frame, so a test can render
/// the same moment twice and diff the buffers. An envelope seeded from a real RNG would make the
/// §B13 flicker rows unmeasurable — two identical frames would differ and the test could not tell
/// that from a repaint bug.
fn envelope(now_ms: u64, period_ms: u64, peak: u8, floor: u8) -> Frame {
    let mut f = BASELINE;
    for (i, cell) in f.iter_mut().enumerate() {
        // Each column lags the one before it, so the frame reads as a wave travelling across the
        // meter rather than twelve independent bars flickering in place.
        let phase = now_ms.wrapping_add((i as u64) * period_ms / SAMPLES as u64) % period_ms;
        let t = phase as f32 / period_ms as f32;
        // Two summed harmonics: a syllable-rate fundamental and a faster overtone. The result has
        // an uneven crest, which is what makes it look like a level meter and not a metronome.
        let a = (t * std::f32::consts::TAU).sin();
        let b = (t * std::f32::consts::TAU * 2.7).sin() * 0.45;
        let mixed = ((a + b) * 0.5 + 0.5).clamp(0.0, 1.0);
        let span = (peak - floor) as f32;
        *cell = floor + (mixed * span).round() as u8;
    }
    f
}

/// Elapsed against expected, as a filled bar. Columns to the left of the front are at full height.
///
/// The front column carries the fractional remainder, so the meter advances smoothly instead of
/// jumping a whole column at a time — twelve discrete steps over twenty minutes would look frozen.
fn progress(elapsed_ms: u64, expected_ms: u64) -> Frame {
    let mut f = BASELINE;
    if expected_ms == 0 {
        return f;
    }
    let fraction = (elapsed_ms as f64 / expected_ms as f64).clamp(0.0, 1.0);
    let exact = fraction * SAMPLES as f64;
    let full = exact.floor() as usize;
    for (i, cell) in f.iter_mut().enumerate() {
        *cell = if i < full {
            LEVELS
        } else if i == full {
            ((exact - full as f64) * LEVELS as f64).round() as u8
        } else {
            0
        };
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_reports_nothing_so_the_indicator_freezes() {
        assert_eq!(sample(StatusState::Waiting, 1_000, 0, 0), None);
    }

    #[test]
    fn idle_reports_a_flat_baseline_which_is_a_reading_not_an_absence() {
        // Still, but live. The user can tell a frozen meter from a live silent one, and §B5 means
        // two different things by them.
        assert_eq!(sample(StatusState::Idle, 1_000, 0, 0), Some(BASELINE));
    }

    #[test]
    fn every_sample_is_within_the_meters_range() {
        for state in [
            StatusState::Listening,
            StatusState::Thinking,
            StatusState::Speaking,
            StatusState::Writing,
            StatusState::Running,
            StatusState::Idle,
        ] {
            for t in (0..4_000).step_by(7) {
                let f = sample(state, t, t, 3_000).expect("state samples");
                for (col, level) in f.iter().enumerate() {
                    assert!(
                        *level <= LEVELS,
                        "{state:?} column {col} at t={t} reported level {level}, above LEVELS={LEVELS}; \
                         the braille encoder would silently clamp and the meter would lie about amplitude"
                    );
                }
            }
        }
    }

    #[test]
    fn the_same_moment_always_produces_the_same_frame() {
        // If this fails, every §B13 flicker measurement becomes unreadable: two identical frames
        // would differ, and the test could not tell that from a real repaint bug.
        for t in [0_u64, 331, 999, 12_345] {
            assert_eq!(
                sample(StatusState::Listening, t, 0, 0),
                sample(StatusState::Listening, t, 0, 0)
            );
        }
    }

    #[test]
    fn running_progress_fills_left_to_right_and_completes() {
        assert_eq!(sample(StatusState::Running, 0, 0, 1_000), Some(BASELINE));
        let done = sample(StatusState::Running, 0, 1_000, 1_000).unwrap();
        assert_eq!(done, [LEVELS; SAMPLES]);
        let half = sample(StatusState::Running, 0, 500, 1_000).unwrap();
        assert_eq!(half[0], LEVELS);
        assert_eq!(half[SAMPLES - 1], 0);
    }

    #[test]
    fn progress_past_the_expected_duration_stays_full_rather_than_wrapping() {
        // A run that overruns its estimate is common. A meter that wrapped to empty would report
        // "just started" for a job that is late, which is the opposite of the truth.
        assert_eq!(
            sample(StatusState::Running, 0, 9_000, 1_000),
            Some([LEVELS; SAMPLES])
        );
    }
}
