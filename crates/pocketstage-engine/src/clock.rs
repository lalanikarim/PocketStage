//! Pure master-clock math (samples ↔ beats). No audio, no I/O — unit-testable
//! without hardware. The audio thread owns the sample counters; this module
//! only converts between time domains.

/// Elapsed beats for a count of audio samples at the given rate and BPM.
pub fn beats_elapsed(samples: u64, rate: u32, bpm: f64) -> f64 {
    samples as f64 * bpm / (rate as f64 * 60.0)
}

/// Elapsed 16th-note steps for a count of audio samples (pattern domain).
pub fn steps_elapsed(samples: u64, rate: u32, bpm: f64) -> f64 {
    beats_elapsed(samples, rate, bpm) * STEPS_PER_BEAT as f64
}

/// 16th notes per beat (R2: fixed 16th-note pattern resolution).
pub const STEPS_PER_BEAT: u32 = 4;

/// 1-based `(bar, beat-in-bar)` for a position in beats. 4 beats per bar;
/// `0.0` is bar 1 beat 1, `4.0` is bar 2 beat 1.
pub fn bar_beat(pos_beats: f64) -> (u32, u32) {
    const BEATS_PER_BAR: u32 = 4;
    let idx = pos_beats.floor().max(0.0) as u64; // 0-based beat index
    (
        (idx / BEATS_PER_BAR as u64 + 1) as u32,
        (idx % BEATS_PER_BAR as u64 + 1) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_length_at_120bpm() {
        // 120 BPM → 1 beat per 0.5 s → 24 000 samples at 48 kHz.
        assert!((beats_elapsed(24_000, 48_000, 120.0) - 1.0).abs() < 1e-9);
        assert!((beats_elapsed(96_000, 48_000, 120.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn beat_length_at_60bpm() {
        // 60 BPM → 1 beat per second.
        assert!((beats_elapsed(48_000, 48_000, 60.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn step_length_at_120bpm() {
        // 120 BPM → 16th note = 125 ms = 6 000 samples at 48 kHz.
        assert!((steps_elapsed(6_000, 48_000, 120.0) - 1.0).abs() < 1e-9);
        assert!((steps_elapsed(96_000, 48_000, 120.0) - 16.0).abs() < 1e-9); // one bar
    }

    #[test]
    fn bar_beat_boundaries() {
        assert_eq!(bar_beat(0.0), (1, 1));
        assert_eq!(bar_beat(0.9), (1, 1));
        assert_eq!(bar_beat(1.0), (1, 2));
        assert_eq!(bar_beat(3.99), (1, 4));
        assert_eq!(bar_beat(4.0), (2, 1));
        assert_eq!(bar_beat(15.999), (4, 4));
        assert_eq!(bar_beat(16.0), (5, 1));
    }
}
