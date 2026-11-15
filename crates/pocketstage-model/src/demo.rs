//! Starter arrangement for a fresh project (what boots when no file exists).
//!
//! Rationale: an empty project is a bad test bed. Every feature that touches
//! scenes — launching them from pads, riding mixer rows while they play,
//! editing steps in the SEQ overlay, punching takes over a loop — needs
//! material that is pleasant enough to leave running for an hour. This module
//! is pure data (no I/O, no audio): it builds clips and scene names, and
//! [Project::default_project] drops them into the four starter tracks.
//!
//! ## The grid shapes the writing
//!
//! Two engine facts decide everything below:
//!
//! 1. **One note per step.** [`crate::StepNote`] is a single pitch, so chords
//!    cannot be stacked — they are *rolled* (one note per 16th, which reads as
//!    a strum) or arpeggiated.
//! 2. **Nothing sustains.** The pattern lane does `note_off(previous step's
//!    pitch)` then `note_on(current step's pitch)`, so a note survives exactly
//!    one step (125 ms at 120 BPM): a rest after it cuts it, and repeating a
//!    pitch re-articulates rather than holds. Consequences that took measuring
//!    (`examples/scene_render_lab.rs` in the engine crate):
//!
//!    * **Level comes from step density, not velocity.** A "pad" written in
//!      quarter notes is ~20 dB below the same figure filling every step, so
//!      every pad scene here fills all 16 steps and only velocity differs.
//!    * **Slow-attack presets are unusable.** FluidR3 "Pad 2 (warm)" (GM 89)
//!      gates to ~−49 dBFS at velocity 74 — its attack is slower than one step
//!      — which is why the starter Pad is GM 91 (Choir Aahs, ~−35 dBFS under
//!      identical gating). Preset choice is a *mixing* decision on this grid.
//!    * Write rhythmic figures; never write "a long note" as one step.
//!
//! ## The music
//!
//! A minor, 120 BPM, four-bar loop, i–VI–III–VII (Am – F – C – G). Four
//! starter tracks with fixed roles: Bass anchors, E.Piano rolls the chords
//! (fact 1), Pad supplies the bed, Piano carries the melody. Scenes are an
//! arrangement, so launching one is a real dynamic change. `rms` is measured,
//! whole-part average from the render lab:
//!
//! | scene   | content                       | quantize   | rms     | what it's good for |
//! |---------|-------------------------------|------------|---------|--------------------|
//! | Intro   | sparse bass + quiet bed       | (default)  | −35.8   | 2-row mixer rides, quiet baseline |
//! | Groove  | bass + rolled chords + pulse  | (default)  | −27.7   | the reference loop |
//! | Full    | + melody                      | NextBeat   | −26.7   | full mix; snap-to-beat launch |
//! | Break   | bass + melody only            | (default)  | −28.1   | solo/mute A/B, take punching |
//! | Climax  | everything, octave up, denser | NextBar    | −25.7   | dynamic jump; snap-to-BAR launch |
//! | Outro   | bed alone                     | (default)  | −34.8   | single-row rides, fade to nothing |
//! | Silence | nothing                       | Immediate  | −65.6   | a real silence cue |
//! | Edit    | 1-bar bass motif + 8th arp    | Immediate  | −28.4   | SEQ-overlay editing (short loop = fast feedback) |
//!
//! Solo parts land within 3 dB of each other (bass −31.5, E.Piano −32.6,
//! Pad −34.4, melody −34.1), i.e. nothing has to be ridden just to hear it,
//! and the mix peaks at 0.35 — headroom left for `master_gain` to boost.
//!
//! The climax variants are the same figures transposed up an octave, which is
//! also a deliberate stress test: it pushes the E.Piano rolls and the pad bed
//! up to G5, so the upper half of the keybed overlaps material that is already
//! playing — the situation where voice stealing and retrigger policy matter.

use crate::{Clip, Pattern, Quantize, STEPS_PER_BAR, Scene, StepNote};

type Hit = (usize, u8, u8); // (step, pitch, velocity)

/// Build a [`Pattern`] from `(step, pitch, velocity)` triples; every step not
/// listed is a rest. `bars * STEPS_PER_BAR` steps, as `validate` requires.
fn pat(bars: u8, hits: &[Hit]) -> Pattern {
    let mut steps = vec![None; bars as usize * STEPS_PER_BAR];
    for &(i, pitch, velocity) in hits {
        // A figure reaching past the end of the loop is a authoring bug: fail
        // loudly in debug (the demo tests run it in CI), but never panic at
        // boot — `default_project` runs before anything can report an error.
        let Some(slot) = steps.get_mut(i) else {
            debug_assert!(
                false,
                "step {i} (pitch {pitch}) past {steps_len}-step pattern",
                steps_len = steps.len()
            );
            continue;
        };
        *slot = Some(StepNote { pitch, velocity });
    }
    Pattern { bars, steps }
}

/// Append a chord as a fast roll: one note per 16th starting at `at`. See the
/// module doc — the grid is monophonic, this is how a chord exists at all.
fn roll(hits: &mut Vec<Hit>, at: usize, notes: &[u8], vel: u8) {
    for (i, n) in notes.iter().enumerate() {
        hits.push((at + i, *n, vel));
    }
}

/// Transpose a figure by `octaves` octaves (velocity preserved).
fn up(octaves: i8, hits: &[Hit]) -> Vec<Hit> {
    let shift = i16::from(octaves) * 12;
    hits.iter()
        .map(|(s, p, v)| (*s, ((i16::from(*p) + shift).clamp(0, 127)) as u8, *v))
        .collect()
}

// ------------------------------------------------------------- track figures

mod bass {
    use super::{Hit, STEPS_PER_BAR};

    /// Am F C G: root, fifth, octave for each bar.
    const CHORDS: [[u8; 3]; 4] = [
        [45, 52, 57], // A2  E3  A3
        [41, 48, 53], // F2  C3  F3
        [48, 55, 60], // C3  G3  C4
        [43, 50, 55], // G2  D3  G3
    ];

    /// Voice a bar-length figure (step, chord-degree, velocity) across the
    /// four-bar progression: degrees are re-voiced per bar so the line follows
    /// the harmony instead of being transposed blindly.
    fn voice(figure: &[(usize, usize, u8)]) -> Vec<Hit> {
        CHORDS
            .iter()
            .enumerate()
            .flat_map(|(bar, notes)| {
                figure
                    .iter()
                    .map(move |(s, deg, v)| (bar * STEPS_PER_BAR + s, notes[*deg], *v))
            })
            .collect()
    }

    /// Root–fifth–octave groove: the reference bass for Groove/Full/Break.
    pub fn groove() -> Vec<Hit> {
        voice(&[
            (0, 0, 110),
            (6, 1, 92),
            (8, 0, 100),
            (12, 2, 88),
            (14, 1, 80),
        ])
    }

    /// Two notes a bar — the Intro's "leave room for everything else".
    pub fn sparse() -> Vec<Hit> {
        voice(&[(0, 0, 88), (10, 2, 68)])
    }

    /// Groove an octave up, plus two extra pushes per bar — the Climax lift.
    pub fn lifted() -> Vec<Hit> {
        let mut hits = super::up(1, &groove())
            .into_iter()
            .map(|(s, p, v)| (s, p, v - 6))
            .collect::<Vec<_>>();
        hits.extend(super::up(1, &voice(&[(4, 0, 76), (10, 2, 74)])));
        hits.sort();
        hits
    }

    /// One bar, four notes: the SEQ-overlay editing bed.
    pub fn edit_motif() -> Vec<Hit> {
        vec![(0, 45, 104), (4, 52, 92), (8, 45, 100), (12, 43, 92)]
    }
}

mod keys {
    use super::Hit;

    /// Am F C G triads, rolled low-to-high.
    const CHORDS: [[u8; 3]; 4] = [
        [57, 60, 64], // A3 C4 E4
        [53, 57, 60], // F3 A3 C4
        [60, 64, 67], // C4 E4 G4
        [55, 59, 62], // G3 B3 D4
    ];

    /// Rolled chord on beat 1 and the "and" of beat 2, per bar.
    pub fn stabs() -> Vec<Hit> {
        let mut hits = Vec::new();
        for (bar, chord) in CHORDS.iter().enumerate() {
            let at = bar * 16;
            super::roll(&mut hits, at, chord, 86);
            super::roll(&mut hits, at + 10, chord, 68);
        }
        hits
    }

    /// Same rolls an octave up with two extra hits — Climax.
    pub fn stabs_lifted() -> Vec<Hit> {
        let mut hits = super::up(1, &stabs());
        for (bar, chord) in CHORDS.iter().enumerate() {
            let at = bar * 16;
            super::roll(&mut hits, at + 4, chord, 62);
            super::roll(&mut hits, at + 13, chord, 58);
        }
        hits.sort();
        hits
    }

    /// Slow pulse: root–fifth–third–octave in 8ths at whisper velocity. A
    /// "pad" in the only sense a one-shot grid can do (module doc, fact 2).
    pub fn pulse() -> Vec<Hit> {
        let mut hits = Vec::new();
        for (bar, chord) in CHORDS.iter().enumerate() {
            let [r, t, f] = [chord[0], chord[1], chord[2]];
            for (i, step) in (0..8).enumerate() {
                let pitch = match i % 4 {
                    0 | 2 => r,
                    1 => f,
                    _ => t,
                };
                hits.push((bar * 16 + step * 2, pitch, 44));
            }
        }
        hits
    }

    /// Continuous bed: EVERY step sounds, the chord tone rotating once a beat.
    ///
    /// Every step sounds; the chord tone rotates once a beat. Measured
    /// (`examples/scene_render_lab.rs`): the identical figure in quarter notes
    /// lands ~15 dB below this one, because the grid cuts each note at the next
    /// step boundary — **level here is step density, not velocity** (module doc,
    /// fact 2). Pad scenes therefore all fill 16 steps and differ only in
    /// velocity: Intro 28 / Outro 36 / Climax 44 an octave up.
    pub fn wash(vel: u8) -> Vec<Hit> {
        let mut hits = Vec::new();
        for (bar, chord) in CHORDS.iter().enumerate() {
            for step in 0..16 {
                hits.push((bar * 16 + step, chord[(step / 4) % 3], vel));
            }
        }
        hits
    }

    /// `wash` an octave up — the Climax swell (denser spectrum, same density).
    pub fn wash_lifted() -> Vec<Hit> {
        super::up(1, &wash(44))
    }
}

mod melody {
    use super::Hit;

    /// Four bars over Am F C G. Pentatonic-ish, ends on B4 (step 60) so it
    /// leans back into A4 at the top of the loop.
    pub fn line() -> Vec<Hit> {
        vec![
            (0, 69, 124),
            (4, 72, 112),
            (6, 76, 120),
            (12, 72, 96),
            (16, 77, 120),
            (20, 72, 108),
            (22, 69, 100),
            (28, 72, 92),
            (32, 76, 122),
            (36, 79, 112),
            (38, 76, 104),
            (44, 72, 96),
            (48, 79, 120),
            (52, 76, 108),
            (54, 74, 100),
            (60, 71, 92),
        ]
    }

    /// The same line an octave up — Climax.
    pub fn line_lifted() -> Vec<Hit> {
        super::up(1, &line())
    }

    /// A4 C5 E5 C5 in 8ths: the SEQ-overlay editing bed (one bar, obvious
    /// grid, easy to hear what a toggled step did).
    pub fn edit_arp() -> Vec<Hit> {
        vec![
            (0, 69, 100),
            (2, 72, 88),
            (4, 76, 100),
            (6, 72, 112),
            (8, 69, 122),
            (10, 72, 112),
            (12, 76, 100),
            (14, 79, 92),
        ]
    }
}

// ------------------------------------------------------------------ assembly

/// The arrangement: `[track][scene] -> clip`. Track order is the starter order
/// from [`crate::Project::default_project`] — 0 Piano, 1 E.Piano, 2 Pad,
/// 3 Bass. Scene order matches [`starter_scenes`].
///
/// Kept as a table (one row per track, eight cells) so adding a scene or
/// re-voicing one clip is a one-line edit.
pub fn starter_clips() -> Vec<Vec<Option<Clip>>> {
    let clip = |name: &str, bars: u8, hits: Vec<Hit>| {
        Some(Clip {
            name: name.into(),
            pattern: pat(bars, &hits),
        })
    };
    let none = || -> Option<Clip> { None };

    // Scene indices: 0 Intro, 1 Groove, 2 Full, 3 Break, 4 Climax,
    //               5 Outro, 6 Silence, 7 Edit.
    vec![
        // Piano — melody only where the tune should carry.
        vec![
            none(),
            none(),
            clip("Melody", 4, melody::line()),
            clip("Melody", 4, melody::line()),
            clip("Melody +8", 4, melody::line_lifted()),
            none(),
            none(),
            clip("Edit arp", 1, melody::edit_arp()),
        ],
        // E.Piano — chord rolls; absent where the mix should feel open.
        vec![
            none(),
            clip("Rolls", 4, keys::stabs()),
            clip("Rolls", 4, keys::stabs()),
            none(),
            clip("Rolls +8", 4, keys::stabs_lifted()),
            none(),
            none(),
            none(),
        ],
        // Pad — the pulse that glues everything; alone in Outro.
        vec![
            clip("Bed", 4, keys::wash(28)),
            clip("Pulse", 4, keys::pulse()),
            clip("Pulse", 4, keys::pulse()),
            clip("Pulse", 4, keys::pulse()),
            clip("Wash +8", 4, keys::wash_lifted()),
            clip("Bed", 4, keys::wash(36)),
            none(),
            none(),
        ],
        // Bass — everywhere except the two atmospheric scenes.
        vec![
            clip("Sparse", 4, bass::sparse()),
            clip("Groove", 4, bass::groove()),
            clip("Groove", 4, bass::groove()),
            clip("Groove", 4, bass::groove()),
            clip("Groove +8", 4, bass::lifted()),
            none(),
            none(),
            clip("Edit motif", 1, bass::edit_motif()),
        ],
    ]
}

/// Scene names + per-scene launch quantize ([`crate::Quantize`], D5) matching
/// the columns of [`starter_clips`]. Two scenes carry explicit overrides so
/// the difference between snap-to-beat and snap-to-bar is audible the first
/// time you launch them; `None` means "use the project default".
pub fn starter_scenes() -> Vec<Scene> {
    [
        ("Intro", None),
        ("Groove", None),
        ("Full", Some(Quantize::NextBeat)),
        ("Break", None),
        ("Climax", Some(Quantize::NextBar)),
        ("Outro", None),
        ("Silence", Some(Quantize::Immediate)),
        ("Edit", Some(Quantize::Immediate)),
    ]
    .iter()
    .map(|(name, quantize)| Scene {
        name: (*name).into(),
        quantize: *quantize,
    })
    .collect()
}
