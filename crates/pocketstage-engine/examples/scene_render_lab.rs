//! Lab: render every scene of an arrangement offline and measure it.
//!
//! Two jobs:
//!
//! 1. **Verify the starter arrangement** (pocketstage-model `demo`) actually
//!    makes sound per scene, at sane levels, with the dynamics its table
//!    promises — Intro quieter than Climax, "Silence" actually silent. Reading
//!    a note table proves the data is well-formed; this proves it plays.
//! 2. **Give a human something to listen to** without touching the engine:
//!    writes one WAV with every scene back to back, so the tunes can be judged
//!    as music instead of as JSON.
//!
//! ```sh
//! cargo run --release -p pocketstage-engine --example scene_render_lab
//! # or: ... scene_render_lab path/to/Project.json [scene-name-for-solo-pass]
//! ```
//!
//! Output: `/tmp/pocketstage-scenes.wav` (or `<input>.wav`). Step gating copies
//! the engine's rule exactly — `note_off(previous step's pitch)` then
//! `note_on(current step's pitch)` — which is why a rest cuts a note, why
//! repeating a pitch re-articulates it, and therefore why **level on this grid
//! comes from step density, not velocity**. Every velocity in the starter
//! arrangement was picked against the two tables this lab prints.

#[path = "../src/ffi.rs"]
mod ffi;

use pocketstage_model::{Project, ProjectFile, STEPS_PER_BAR};

const RATE: usize = 48_000;
const SF2: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";
const GAP_SECS: f64 = 0.25;

struct Pcm {
    data: Vec<f32>,
}

impl Pcm {
    fn push(&mut self, l: f32, r: f32) {
        self.data.push(l);
        self.data.push(r);
    }
    fn silence(&mut self, frames: usize) {
        self.data.resize(self.data.len() + frames * 2, 0.0);
    }
}

fn write_wav(path: &str, pcm: &Pcm) -> std::io::Result<()> {
    let bytes = (pcm.data.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.data.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&(RATE as u32).to_le_bytes());
    out.extend_from_slice(&(RATE as u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    for s in &pcm.data {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, out)
}

fn dbfs(v: f64) -> f64 {
    if v <= 1e-9 { -180.0 } else { 20.0 * v.log10() }
}

/// Render one scene with the engine's step gating. `only = Some(t)` renders a
/// single track (the solo pass that made the velocity table possible).
fn render(
    synth: &ffi::Synth,
    project: &Project,
    scene: usize,
    only: Option<usize>,
    samples_per_step: usize,
    mut capture: Option<&mut Pcm>,
) -> (usize, f64, f32) {
    let steps = project
        .tracks
        .iter()
        .enumerate()
        .filter(|(t, _)| only.is_none_or(|want| *t == want))
        .filter_map(|(_, t)| t.clips.get(scene).and_then(|c| c.as_ref()))
        .map(|c| c.pattern.steps.len())
        .max()
        .unwrap_or(STEPS_PER_BAR);

    let mut notes = 0usize;
    let mut rms_sum = 0f64;
    let mut peak = 0f32;
    let mut l = vec![0f32; samples_per_step];
    let mut r = vec![0f32; samples_per_step];

    for k in 0..steps {
        for (chan, track) in project.tracks.iter().enumerate() {
            if only.is_some_and(|want| chan != want) {
                continue;
            }
            let Some(clip) = track.clips.get(scene).and_then(|c| c.as_ref()) else {
                continue;
            };
            if track.mute {
                continue;
            }
            let len = clip.pattern.steps.len();
            if let Some(prev) = clip.pattern.steps[(k + len - 1) % len].as_ref() {
                synth.note_off(chan as i32, prev.pitch as i32);
            }
            if let Some(n) = clip.pattern.steps[k % len].as_ref() {
                synth.note_on(chan as i32, n.pitch as i32, n.velocity as i32);
                notes += 1;
            }
        }
        synth.render(samples_per_step, &mut l, &mut r);

        for (a, b) in l.iter().zip(r.iter()) {
            rms_sum += (a * a + b * b) as f64 / 2.0;
            peak = peak.max(a.abs()).max(b.abs());
            if let Some(p) = capture.as_deref_mut() {
                p.push(*a, *b);
            }
        }
    }
    // Kill everything before moving on: samples keep decaying after note_off,
    // and without this the previous pass bleeds into the next one (which made
    // the "Silence" scene measure loud).
    for chan in 0..project.tracks.len() as i32 {
        synth.all_sounds_off(chan);
    }
    (
        notes,
        (rms_sum / (steps * samples_per_step) as f64).sqrt(),
        peak,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let project: Project = match args.get(1) {
        Some(p) => {
            let text = std::fs::read_to_string(p).expect("read project");
            ProjectFile::from_json_str(&text).expect("parse").project
        }
        None => Project::default_project(),
    };

    let settings = ffi::Settings::new().expect("settings");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.3); // engine boot default
    let synth = ffi::Synth::new(&settings).expect("synth");
    synth.load_soundfont(SF2).expect("sf2");

    for (i, t) in project.tracks.iter().enumerate() {
        synth.program_change(i as i32, t.instrument.gm_program as i32);
        // Mirror the engine's mixer defaults so levels mean what they'll mean live.
        let gain = t.gain.unwrap_or(1.0).clamp(0.0, 1.0);
        synth.cc(i as i32, 7, (gain * 127.0).round() as i32);
    }

    let samples_per_step = (RATE as f64 * 60.0 / project.bpm / 4.0).round() as usize;
    let mut pcm = Pcm { data: Vec::new() };

    println!("=== per scene (full mix)");
    println!(
        "{:<9} {:>6} {:>10} {:>7}   tracks",
        "scene", "notes", "rms dBFS", "peak"
    );
    for (s, scene) in project.scenes.iter().enumerate() {
        let playing: Vec<&str> = project
            .tracks
            .iter()
            .enumerate()
            .filter(|(t, _)| {
                project.tracks[*t]
                    .clips
                    .get(s)
                    .map(|c| c.is_some())
                    .unwrap_or(false)
            })
            .map(|(_, t)| t.name.as_str())
            .collect();
        let (notes, rms, peak) =
            render(&synth, &project, s, None, samples_per_step, Some(&mut pcm));
        pcm.silence((GAP_SECS * RATE as f64) as usize);
        println!(
            "{:<9} {:>6} {:>10.1} {:>7.3}   {}",
            scene.name,
            notes,
            dbfs(rms),
            peak,
            playing.join(", ")
        );
    }

    // Solo pass: the only way to mix by number instead of by ear-throwing-darts.
    let solo_scene = args
        .get(2)
        .and_then(|name| {
            project
                .scenes
                .iter()
                .position(|sc| sc.name.eq_ignore_ascii_case(name))
        })
        .unwrap_or(2);
    println!(
        "\n=== solo, scene '{}' (part-average RMS: what the ear integrates)",
        project.scenes[solo_scene].name
    );
    for (t, track) in project.tracks.iter().enumerate() {
        if track
            .clips
            .get(solo_scene)
            .and_then(|c| c.as_ref())
            .is_none()
        {
            continue;
        }
        let (notes, rms, peak) = render(
            &synth,
            &project,
            solo_scene,
            Some(t),
            samples_per_step,
            None,
        );
        println!(
            "  {:<9} prog {:>3} {:>4} notes  {:>7.1} dBFS  peak {:.3}",
            track.name,
            track.instrument.gm_program,
            notes,
            dbfs(rms),
            peak
        );
    }

    synth.all_sounds_off(0);
    let out = match args.get(1) {
        Some(p) => format!("{p}.wav"),
        None => "/tmp/pocketstage-scenes.wav".into(),
    };
    write_wav(&out, &pcm).expect("write wav");
    println!(
        "\nlistenable render -> {out}  ({:.1} s)",
        pcm.data.len() as f64 / 2.0 / RATE as f64
    );
}
