//! Pure transport & scene-launch decision logic — no synth, no channels,
//! no syscalls: everything here is unit-testable and CANNOT panic.
//!
//! This extraction exists because the render loop grew real state-machine
//! logic (quantized launches, position gating) while remaining untestable
//! in place. A `LaunchScene{4}` against a 3-scene project once indexed
//! `project.scenes[4]` inside the audio thread and killed all sound while
//! the process kept serving HTTP — silent-but-alive, the worst failure to
//! debug. Total functions with explicit `Option`s make that class of bug
//! a compile error instead.

use pocketstage_model::Quantize;

/// What the caller must do in response to a launch request. The engine
/// performs the side effects (synth program changes, clock start); this
/// module only decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Clock was stopped: start playback now with this scene active.
    Start { scene: usize },
    /// Clock running, quantize=Immediate: switch the active scene now.
    Switch { scene: usize },
    /// Waiting for the quantize boundary (`tick` reports when).
    Queued,
    /// The requested scene does not exist in the current project — do
    /// absolutely nothing (the scenes-panic regression guard).
    Ignore,
}

/// Transport/scene state of the audio thread. `playing` mirrors whether
/// the master clock runs; sample-domain bookkeeping stays in main.rs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Transport {
    pub playing: bool,
    pub active: Option<usize>,
    pending: Option<(usize, Quantize)>,
}

impl Transport {
    /// Request scene `index`. `q` is the scene's launch quantize, or
    /// `None` when the project has no such scene (out of range) — which
    /// is ignored, never applied. Launching from a stopped transport
    /// always starts immediately (D5), regardless of quantize.
    pub fn launch(&mut self, index: usize, q: Option<Quantize>) -> Effect {
        let Some(q) = q else { return Effect::Ignore };
        if !self.playing {
            self.playing = true; // caller starts the clock on Effect::Start
            self.pending = None;
            self.active = Some(index);
            Effect::Start { scene: index }
        } else if q == Quantize::Immediate {
            self.pending = None;
            self.active = Some(index);
            Effect::Switch { scene: index }
        } else {
            self.pending = Some((index, q));
            Effect::Queued
        }
    }

    /// ⏹ — scene state is fully reset (stop means silent; Play restarts
    /// the take loop from the top with no scene).
    pub fn stop(&mut self) {
        self.playing = false;
        self.active = None;
        self.pending = None;
    }

    /// Advance: `beat` is an integer beat position the playhead just
    /// reached. Returns the scene index to activate when a pending launch
    /// hits its boundary (NextBar on multiples of 4, NextBeat on any beat).
    pub fn tick(&mut self, beat: u64) -> Option<usize> {
        let (index, q) = self.pending?;
        let fire = match q {
            Quantize::Immediate => true, // never queued, but be total
            Quantize::NextBeat => true,
            Quantize::NextBar => beat.is_multiple_of(4),
        };
        if fire {
            self.pending = None;
            self.active = Some(index);
            Some(index)
        } else {
            None
        }
    }

    /// A project swap may have shrunk the scene list. Drop an active or
    /// pending launch that no longer exists. Returns true if anything was
    /// cleared (caller zeroes its live-status atomic).
    pub fn clamp_scenes(&mut self, scenes_len: usize) -> bool {
        let mut cleared = false;
        if self.active.is_some_and(|i| i >= scenes_len) {
            self.active = None;
            cleared = true;
        }
        if self.pending.is_some_and(|(i, _)| i >= scenes_len) {
            self.pending = None;
            cleared = true;
        }
        cleared
    }

    /// 16th-note step window for pattern gating inside one render period.
    /// Given fractional step positions `s0` (start of period) and `s1`
    /// (end), return `(first, last_excl)` — the steps *reached* during
    /// the period, i.e. k in ceil(s0)..(first k >= s1). Empty if none.
    pub fn step_window(s0: f64, s1: f64) -> (u64, u64) {
        let first = s0.ceil().max(0.0) as u64;
        let last_excl = if s1 <= 0.0 { 0 } else { s1.ceil() as u64 };
        // original loop: `while (k as f64) < s1` — an exact-integer s1 is
        // exclusive, so ceil overshoot only when s1 has a fraction.
        let last_excl = if (s1.fract() == 0.0) && s1 >= 0.0 {
            s1 as u64
        } else {
            last_excl
        };
        (first, last_excl.max(first))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NB: Option<Quantize> = Some(Quantize::NextBar);
    const NBEAT: Option<Quantize> = Some(Quantize::NextBeat);
    const IMM: Option<Quantize> = Some(Quantize::Immediate);

    #[test]
    fn launch_of_missing_scene_is_ignored() {
        // Regression: this exact mistake panicked the audio thread live.
        let mut t = Transport::default();
        assert_eq!(t.launch(4, None), Effect::Ignore);
        assert_eq!(t.active, None);
        assert_eq!(t.tick(8), None);
    }

    #[test]
    fn launch_from_stopped_starts_immediately_any_quantize() {
        let mut t = Transport::default();
        assert_eq!(t.launch(2, NB), Effect::Start { scene: 2 });
        assert_eq!(t.active, Some(2));
        assert!(t.playing, "Start implies the clock runs afterwards");
        // A second launch while running must NOT be another Start.
        assert_eq!(t.launch(3, NB), Effect::Queued);
    }

    #[test]
    fn immediate_switches_while_playing() {
        let mut t = Transport {
            playing: true,
            ..Default::default()
        };
        assert_eq!(t.launch(1, IMM), Effect::Switch { scene: 1 });
        assert_eq!(t.active, Some(1));
    }

    #[test]
    fn next_bar_pending_fires_only_on_bar_boundary() {
        let mut t = Transport {
            playing: true,
            ..Default::default()
        };
        assert_eq!(t.launch(3, NB), Effect::Queued);
        assert_eq!(t.active, None, "queued must not switch yet");
        assert_eq!(t.tick(5), None);
        assert_eq!(t.tick(6), None);
        assert_eq!(t.tick(7), None);
        assert_eq!(t.tick(8), Some(3));
        assert_eq!(t.active, Some(3));
        assert_eq!(t.tick(12), None, "consumed once");
    }

    #[test]
    fn next_beat_pending_fires_on_any_beat() {
        let mut t = Transport {
            playing: true,
            ..Default::default()
        };
        assert_eq!(t.launch(0, NBEAT), Effect::Queued);
        assert_eq!(t.tick(5), Some(0));
    }

    #[test]
    fn newer_launch_replaces_pending() {
        let mut t = Transport {
            playing: true,
            ..Default::default()
        };
        t.launch(1, NB);
        t.launch(2, NB);
        assert_eq!(t.tick(8), Some(2));
    }

    #[test]
    fn stop_resets_scene_and_pending() {
        let mut t = Transport {
            playing: true,
            ..Default::default()
        };
        t.launch(1, NB); // queued
        t.stop();
        assert!(!t.playing);
        assert_eq!(t.active, None);
        assert_eq!(t.tick(8), None);
    }

    #[test]
    fn clamp_drops_dangling_active_and_pending() {
        let mut t = Transport {
            playing: true,
            active: Some(5),
            ..Default::default()
        };
        assert!(t.clamp_scenes(3));
        assert_eq!(t.active, None);

        let mut t = Transport {
            playing: true,
            ..Default::default()
        };
        t.launch(4, NB); // pending 4, project then shrinks to 3
        assert!(t.clamp_scenes(3));
        assert_eq!(t.tick(8), None, "dangling pending must never fire");

        let mut t = Transport {
            playing: true,
            active: Some(2),
            ..Default::default()
        };
        assert!(!t.clamp_scenes(3), "in-range survives");
        assert_eq!(t.active, Some(2));
    }

    #[test]
    fn step_window_matches_the_original_loop() {
        // original: k = ceil(s0); while (k as f64) < s1
        let w = |s0: f64, s1: f64| {
            let first = s0.ceil() as u64;
            let mut got = vec![];
            let mut k = first;
            while (k as f64) < s1 {
                got.push(k);
                k += 1;
            }
            got
        };
        let cases: [(f64, f64); 7] = [
            (0.0, 0.75),
            (0.75, 1.2),
            (3.0, 4.0),
            (3.2, 4.0), // period ends exactly on step 4 -> exclusive
            (3.0, 4.001),
            (10.9, 11.1),
            (5.0, 5.0),
        ];
        for &(s0, s1) in &cases {
            let (f, e) = Transport::step_window(s0, s1);
            let mine: Vec<u64> = (f..e).collect();
            assert_eq!(mine, w(s0, s1), "s0={s0} s1={s1}");
        }
    }
}
