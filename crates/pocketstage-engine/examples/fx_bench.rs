//! Session-10 post-mortem lab: "pressing and holding a key sounds stacked /
//! uncrisp; tapping is crisp". Headless — pure FFI render, no ALSA device.
//!
//! Verdict after measurement (all numbers below are sample-RMS distances
//! relative to the stock sound, or tail/sustain energy ratios):
//!
//! 1. The chorus FFI signature WAS wrong in session 10 (five args declared
//!    for a six-arg C function — real order `(synth, nr, level, speed,
//!    depth_ms, type)`), so every boot applied garbage chorus params (the
//!    "speed must be below 5 Hz" warning proved it). But FluidR3 GM piano /
//!    organ / strings have ZERO chorus send — the broken call was inaudible
//!    on exactly the sound the user complained about.
//! 2. The actual culprit: session 10 also called, unconditionally at boot,
//!    `fluid_synth_set_reverb(0.5, 0.4, 0.7, 0.9)` — while this build's
//!    stock reverb is room=0.2 damp=0.0 width=0.5. More than double the
//!    room size → long accumulating tail; held notes pile onto their own
//!    reverb ("stacking"), taps stay crisp. Run this lab to see it.
//!
//! Fix: FX knobs are LEVEL-ONLY and OPT-IN (`None` = touch nothing); boot
//! writes no effect params at all. See core.rs SynthBackend::set_global_fx.
//!
//! Run: `cargo run --release -p pocketstage-engine --example fx_bench`

#[path = "../src/ffi.rs"]
mod ffi;

const RATE: usize = 48_000;
const SF2: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

unsafe extern "C" {
    fn fluid_synth_set_reverb(
        s: *mut ffi::fluid_synth_t,
        roomsize: f64,
        damping: f64,
        width: f64,
        level: f64,
    ) -> i32;
}

/// Which boot config to render under.
enum Config {
    /// Untouched FluidSynth — what the engine does now for fx-less projects.
    Stock,
    /// What session 10 did at boot: forced reverb params (+ broken chorus).
    Session10Boot,
    /// Fixed engine with both knobs explicitly set (project says 0.9 / off).
    Knobs(f64, f64),
}

fn render(prog: i32, cfg: Config) -> Vec<f32> {
    let settings = ffi::Settings::new().expect("settings");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.05);
    let synth = ffi::Synth::new(&settings).expect("synth");
    synth.load_soundfont(SF2).expect("sf2");
    match cfg {
        Config::Stock => {}
        Config::Session10Boot => unsafe {
            fluid_synth_set_reverb(synth.0, 0.5, 0.4, 0.7, 0.9);
        },
        Config::Knobs(r, c) => synth.set_global_fx(Some(r), Some(c)),
    }
    synth.program_change(0, prog);
    synth.note_on(0, 60, 100);
    let mut l = vec![0f32; 4 * RATE];
    let mut r = vec![0f32; 4 * RATE];
    synth.render(3 * RATE, &mut l, &mut r); // 3 s held
    synth.note_off(0, 60);
    synth.render(RATE, &mut l[3 * RATE..], &mut r[3 * RATE..]); // 1 s tail
    l
}

fn rms(v: &[f32]) -> f64 {
    (v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / v.len() as f64).sqrt()
}

/// Post-note-off energy vs sustain energy — the "wash" a held note leaves.
fn tail_ratio(v: &[f32]) -> f64 {
    let sus = rms(&v[9_600..3 * RATE]); // skip 200 ms attack
    let tail = rms(&v[3 * RATE + 4_800..]); // skip 100 ms release
    if sus > 0.0 { tail / sus } else { 0.0 }
}

fn dist(a: &[f32], b: &[f32]) -> f64 {
    rms(&a.iter().zip(b).map(|(x, y)| x - y).collect::<Vec<_>>()) / rms(b).max(1e-12)
}

fn main() {
    if !std::path::Path::new(SF2).exists() {
        eprintln!("soundfont missing: {SF2}");
        std::process::exit(1);
    }
    println!("held C4 for 3 s, released; wash = tail/sustain energy\n");
    for (name, prog) in [("Grand Piano", 0), ("Strings", 48)] {
        let stock = render(prog, Config::Stock);
        let s10 = render(prog, Config::Session10Boot);
        let knobs = render(prog, Config::Knobs(0.9, 0.0));
        println!(
            "{name}: session-10 boot vs stock: dist={:.3} wash {:.2}→{:.2}",
            dist(&s10, &stock),
            tail_ratio(&stock),
            tail_ratio(&s10)
        );
        println!(
            "{name}: fixed knobs(0.9/off) vs stock: dist={:.3} wash={:.2}",
            dist(&knobs, &stock),
            tail_ratio(&knobs)
        );
    }
    // Chorus sanity: Lead Synth 81 is the probe instrument WITH a chorus
    // send — proves set_global_fx's chorus path (fixed FFI) does act.
    let stock = render(81, Config::Stock);
    let choff = render(81, Config::Knobs(0.9, 0.0));
    println!(
        "Lead Synth: chorus explicitly off vs stock: dist={:.3}",
        dist(&choff, &stock)
    );
}
