//! Lab: what does FluidSynth actually do with an OUT-OF-RANGE CC value?
//!
//! Born from D33. `Track::gain` validated up to 2.0 and the engine mapped it
//! straight to CC7 (`gain × 127`), so a "boosted" track sent CC7 = 203 and
//! users heard a dead slider: right of center did nothing. Two candidate
//! explanations look the same from the sofa but are not:
//!
//!   * **clamped** → the channel snaps to full level at 127; or
//!   * **rejected** → the message is dropped and the channel KEEPS its
//!     previous level, i.e. the fader sticks at whatever it was before.
//!
//! Run it (offline render, no audio device, safe next to a running engine):
//!
//! ```sh
//! cargo run --release -p pocketstage-engine --example cc_range_lab
//! ```
//!
//! A pad is held for the whole run and one window is rendered per step; the
//! RMS of each window's last 300 ms is printed in dBFS. If "rejected" is the
//! truth, the `CC7=203` window matches the preceding cut, and going back to
//! 127 restores full level.

#[path = "../src/ffi.rs"]
mod ffi;

const RATE: usize = 48_000;
const SF2: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

fn rms_dbfs(l: &[f32], r: &[f32]) -> f64 {
    let n = (l.len() * 2) as f64;
    let sum: f64 = l
        .iter()
        .chain(r.iter())
        .map(|s| (*s as f64) * (*s as f64))
        .sum();
    let rms = (sum / n).sqrt();
    if rms <= 1e-9 {
        -180.0
    } else {
        20.0 * rms.log10()
    }
}

fn main() {
    let settings = ffi::Settings::new().expect("settings");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.3); // engine boot default (main.rs)
    let synth = ffi::Synth::new(&settings).expect("synth");
    synth.load_soundfont(SF2).expect("sf2");

    // Warm pad: sustains audibly for seconds while the key is held, so each
    // window reflects a steady-state level rather than an attack transient.
    synth.program_change(0, 89);
    synth.note_on(0, 60, 110);

    let steps = [
        ("CC7=127 (unity reference)", 127),
        ("CC7=10  (hard cut)", 10),
        ("CC7=203 (OUT OF RANGE)", 203),
        ("CC7=127 (reference again)", 127),
    ];

    let mut l = vec![0f32; RATE];
    let mut r = vec![0f32; RATE];
    println!("held pad, one second per step (RMS of the last 300 ms):");
    for (label, value) in steps {
        synth.cc(0, 7, value);
        // Render ~1.75 s so the level settles (and any rejected CC has had its
        // chance to take effect), then measure a final second's tail.
        let mut sa = vec![0f32; RATE / 4];
        let mut sb = vec![0f32; RATE / 4];
        for _ in 0..7 {
            synth.render(RATE / 4, &mut sa, &mut sb);
        }
        synth.render(RATE, &mut l, &mut r);
        let tail = RATE * 7 / 10;
        println!(
            "  {label:32} {:>7.1} dBFS",
            rms_dbfs(&l[tail..], &r[tail..])
        );
    }
    // Safety: silence the pad before the synth goes away.
    synth.note_off(0, 60);
    synth.all_sounds_off(0);
}
