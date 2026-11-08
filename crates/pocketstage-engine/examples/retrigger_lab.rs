//! Lab: prove the retrigger kill mechanism (D17) with the engine's own FFI
//! wrappers. Crash on ch10 + ride on ch15 (both drum-type), All Sound Off on
//! ch10 at 400 ms — the crash must stop instantly while the ride keeps
//! ringing. Findings: one-shot GM voices ignore note-off AND live
//! GEN_ATTENUATION edits; CC120 per-channel is the only reliable kill, hence
//! one channel per pad in the engine.
//!
//! Run: `cargo run --release -p pocketstage-engine --example retrigger_lab`

#[path = "../src/ffi.rs"]
mod ffi;

const RATE: usize = 48_000;
const BLOCK: usize = 64;

fn main() {
    let settings = ffi::Settings::new().expect("settings");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.5);
    let synth = ffi::Synth::new(&settings).expect("synth");
    synth
        .load_soundfont("/usr/share/sounds/sf2/FluidR3_GM.sf2")
        .expect("sf2");
    for ch in [10i32, 15] {
        synth.set_channel_type_drum(ch);
    }

    let mut left = vec![0f32; BLOCK];
    let mut right = vec![0f32; BLOCK];
    // Phase A: crash on ch10 alone -> all_sounds_off(10) at 400ms must zero it.
    // Phase B (after the kill): ride on ch15 starts at 800ms -> unaffected by
    // the already-fired ch10 sound-off, keeps ringing to the end.
    synth.note_on(10, 49, 110);
    let (mut killed, mut rode) = (false, false);

    print!("A: crash ch10 killed @400ms | B: ride ch15 starts @800ms — peaks: ");
    for t_ms in (0..=2500u64).step_by(50) {
        if !killed && t_ms >= 400 {
            synth.all_sounds_off(10);
            killed = true;
        }
        if !rode && t_ms >= 800 {
            synth.note_on(15, 51, 110);
            rode = true;
        }
        synth.render(BLOCK, &mut left, &mut right);
        let mut p = 0.0f32;
        for i in 0..BLOCK {
            p = p.max(left[i].abs()).max(right[i].abs());
        }
        print!("{:>5.2}s {p:.3} | ", t_ms as f64 / 1000.0);
        if t_ms % 1000 == 950 {
            println!();
        }
    }
    println!();
}
