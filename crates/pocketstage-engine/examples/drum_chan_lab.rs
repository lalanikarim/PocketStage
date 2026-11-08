//! Lab: how do channels 8..=15 actually resolve to percussion presets?
//! Variant A = set_channel_type only. Variant B = type + bank_select(128) +
//! program_change(0). Plays closed hi-hat (42) on ch10 and reports the tail
//! ratio — a drum one-shot is gone after ~0.5 s (ratio ≈ 0), while an
//! unresolved melodic preset (piano on key 42) sustains (ratio > 0.05).
//!
//! Run: `cargo run --release -p pocketstage-engine --example drum_chan_lab`

#[path = "../src/ffi.rs"]
mod ffi;

const RATE: usize = 48_000;
const BLOCK: usize = 256;

fn tail_ratio(with_bank: bool) -> f32 {
    let settings = ffi::Settings::new().expect("settings");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.5);
    let synth = ffi::Synth::new(&settings).expect("synth");
    synth
        .load_soundfont("/usr/share/sounds/sf2/FluidR3_GM.sf2")
        .expect("sf2");
    let ch = 10i32;
    let rc_type = unsafe { ffi::fluid_synth_set_channel_type(synth.0, ch, 1) };
    if with_bank {
        synth.bank_select(ch, 128);
        synth.program_change(ch, 0);
    }
    eprintln!(
        "[lab] variant {}: set_channel_type rc={rc_type}",
        if with_bank {
            "B (bank128+prog0)"
        } else {
            "A (type only)"
        }
    );

    let mut left = vec![0f32; BLOCK];
    let mut right = vec![0f32; BLOCK];
    synth.note_on(ch, 42, 110); // closed hi-hat
    let (mut head, mut tail) = (0.0f32, 0.0f32);
    let n_blocks = RATE * 2 / BLOCK;
    for b in 0..n_blocks {
        synth.render(BLOCK, &mut left, &mut right);
        let e: f32 = left.iter().map(|s| s * s).sum::<f32>().sqrt();
        if b < RATE * 3 / 10 / BLOCK {
            head = head.max(e);
        } else if b > RATE / BLOCK {
            tail = tail.max(e);
        }
    }
    if head > 1e-9 { tail / head } else { 0.0 }
}

fn main() {
    let a = tail_ratio(false);
    let b = tail_ratio(true);
    println!(
        "A type-only   tail ratio {a:.4}  {}",
        if a > 0.05 {
            "<= SUSTAINED: melodic!"
        } else {
            "<= one-shot"
        }
    );
    println!(
        "B bank128+pr0 tail ratio {b:.4}  {}",
        if b > 0.05 {
            "<= SUSTAINED: melodic!"
        } else {
            "<= one-shot"
        }
    );
}
