//! Ground-truth reference render: exactly what the live engine should emit
//! for a GM program on the lead channel — same hand-written FFI, same
//! soundfont, same master gain (0.3) as `main.rs`.
//!
//! Use it to A/B against the hardware: pick program N in the web voice
//! dropdown, play E4 G4 C5 on the keys, and compare with
//!
//! ```sh
//! cargo run --release -p pocketstage-engine --example voice_ref N
//! aplay /tmp/voice_ref_N.wav   # raw `hw:` device — plughw may be busy
//! ```
//!
//! (Session 13: this lab caught a mislabelled web dropdown — the engine
//! played program N correctly; the UI's hardcoded name table lied. Read
//! names from the font via GET /api/presets, never hand-type them.)
//!
//! If the WAV sounds right but the keyboard doesn't, the divergence is in
//! the MIDI path or the unit (local sounds, octave, held notes) — not the
//! soundfont/program mapping.

#[path = "../src/ffi.rs"]
mod ffi;

const RATE: usize = 48_000;
const SF2: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(112);
    if !(0..=127).contains(&prog) {
        eprintln!("program must be 0..=127, got {prog}");
        std::process::exit(1);
    }
    let out = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| format!("/tmp/voice_ref_{prog}.wav"));

    let settings = ffi::Settings::new().expect("settings");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.3); // engine boot default (main.rs)
    let synth = ffi::Synth::new(&settings).expect("synth");
    synth.load_soundfont(SF2).expect("sf2");
    println!("program {prog} on channel 0, notes E4 G4 C5");
    synth.program_change(0, prog);

    // Timeline: (sample, note_on?) for E4 G4 C5 — 300 ms each, 150 ms gaps,
    // 600 ms tail so one-shots/sustains can finish.
    let note_ms = 300usize;
    let gap_ms = 150usize;
    let notes = [64i32, 67, 72];
    let mut events: Vec<(usize, i32, bool)> = Vec::new();
    let mut t = RATE / 10; // 100 ms lead-in silence
    for n in notes {
        let on_at = t;
        let off_at = t + note_ms * RATE / 1000;
        events.push((on_at, n, true));
        events.push((off_at, n, false));
        t = off_at + gap_ms * RATE / 1000;
    }
    let total = t + 600 * RATE / 1000;

    let mut l = vec![0f32; total];
    let mut r = vec![0f32; total];
    let mut ei = 0;
    let mut s = 0usize;
    while s < total {
        while let Some(&(at, note, on)) = events.get(ei)
            && at <= s
        {
            if on {
                synth.note_on(0, note, 90);
            } else {
                synth.note_off(0, note);
            }
            ei += 1;
        }
        let n = (total - s).min(256);
        synth.render(n, &mut l[s..s + n], &mut r[s..s + n]);
        s += n;
    }

    // Interleave to S16LE — i16::MAX scale, NEVER i32 (session-10c rule).
    let mut pcm = Vec::with_capacity(total * 4);
    for (a, b) in l.iter().zip(r.iter()) {
        pcm.extend_from_slice(&((a.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
        pcm.extend_from_slice(&((b.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + pcm.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&0x0001u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&(RATE as u32).to_le_bytes());
    wav.extend_from_slice(&(RATE as u32 * 4).to_le_bytes()); // byte rate
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    std::fs::write(&out, wav).expect("write wav");
    println!("wrote {out}");
}
