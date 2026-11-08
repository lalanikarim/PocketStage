//! PocketStage — audio engine (spike validated; now wired to real MIDI).
//!
//! Architecture (scratchpad R5):
//!   * One FluidSynth instance, owned exclusively by the audio thread.
//!   * Single writer: everything else sends `Event`s across an mpsc channel;
//!     no locks, no allocation on the render path.
//!   * ALSA S16LE stereo, 256-frame periods x 3 @ 48 kHz, underrun recovery.
//!
//! Event sources:
//!   * default: live MIDI — `spawn_pump` → per-port Oxygen mappers (music +
//!     HUI) → engine events. Keys drive track 1's voice; pads 1–8 launch
//!     scenes (their clip column, D5-quantized); transport drives the master
//!     clock (accented beat ticks while playing, position in stats).
//!   * The boot project is `Project::default_project()` with placeholder demo
//!     patterns in scenes 1–2; real project loading comes with the web server.
//!   * `--demo`: scripted arpeggio feeder, no hardware needed (spike mode).
//!
//! Run:  cargo run --release -p pocketstage-engine            (live MIDI)
//!       cargo run --release -p pocketstage-engine -- --demo  (no hardware)
//! ALSA device: positional arg > `POCKETSTAGE_DEVICE` env var > `default`.

mod clock;
mod ffi;

use alsa::pcm::{Access, Format, Frames, PCM};
use alsa::{Direction, ValueOr};
use pocketstage_midi::{PumpMessage, spawn_pump};
use pocketstage_model::{Clip, Pattern, Project, Quantize, STEPS_PER_BAR, StepNote};
use pocketstage_oxygen::{Action, Mapper, default_mapping, hui_mapping};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- constants

const RATE: u32 = 48_000;
const PERIOD_FRAMES: usize = 256; // frames per period (buffer size knob)
const N_PERIODS: u32 = 3;
const SF2_PATH: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

/// Keys play track 1's voice (D3: track N → internal channel N).
const LEAD_CH: i32 = 0;
/// GM drum channel (channel 10 in MIDI land, index 9) — metronome ticks.
const DRUM_CH: i32 = 9;

// ------------------------------------------------------------------- events

enum Event {
    NoteOn {
        chan: i32,
        key: i32,
        vel: i32,
    },
    NoteOff {
        chan: i32,
        key: i32,
    },
    /// Transport: start playback — master clock runs from the current sample.
    Play,
    /// Transport: stop playback — clock halts; position restarts on next play.
    Stop,
    /// Launch scene `index` (0-based). Stopped → starts playing immediately;
    /// playing → applied at the scene's quantized boundary (D5).
    LaunchScene {
        index: usize,
    },
}

/// An event plus the wall-clock time it was *intended* to fire. The audio
/// thread records (consume_time - intended) as scheduling latency.
struct TimedEvent {
    intended: Instant, // when the feeder meant to emit it (feeder jitter = send - intended)
    sent: Instant,     // when it hit the channel (engine latency = consume - sent)
    event: Event,
}

// -------------------------------------------------------------------- stats

#[derive(Default)]
struct Stats {
    cycles: AtomicU64,           // render+write iterations
    busy_ns: AtomicU64,          // time spent in synth render + interleave (not blocking writes)
    xruns: AtomicU64,            // ALSA underruns recovered
    events: AtomicU64,           // MIDI events consumed
    q_sum_us: AtomicU64,         // engine-internal queue wait: consumed - sent
    q_max_us: AtomicU64,         // worst queue wait in window
    feed_max_us: AtomicU64,      // worst feeder jitter (sent - intended) in window
    delay_frames_max: AtomicU64, // max snd_pcm_delay seen in window (output-side queue)
    // Transport *levels* (read by the stats thread, never reset):
    playing: std::sync::atomic::AtomicBool,
    pos_beats_x1000: std::sync::atomic::AtomicU32,
    /// Active scene, stored 1-based; 0 = none.
    active_scene: std::sync::atomic::AtomicU8,
}

impl Stats {
    fn snapshot_and_reset(&self) -> [u64; 8] {
        [
            self.cycles.swap(0, Ordering::Relaxed),
            self.busy_ns.swap(0, Ordering::Relaxed),
            self.xruns.swap(0, Ordering::Relaxed),
            self.events.swap(0, Ordering::Relaxed),
            self.q_sum_us.swap(0, Ordering::Relaxed),
            self.q_max_us.swap(0, Ordering::Relaxed),
            self.feed_max_us.swap(0, Ordering::Relaxed),
            self.delay_frames_max.swap(0, Ordering::Relaxed),
        ]
    }
}

// --------------------------------------------------------------------- main

fn main() {
    // Args: optional ALSA device and `--demo` to run the scripted feeder
    // instead of live MIDI. Device precedence: positional arg > POCKETSTAGE_DEVICE
    // > "default" (PipeWire plugin on the dev box; plain ALSA on the Pi).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let demo = args.iter().any(|a| a == "--demo");
    let device = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .or_else(|| std::env::var("POCKETSTAGE_DEVICE").ok())
        .unwrap_or_else(|| "default".to_string());

    // ---- FluidSynth setup (main thread; handles move to the audio thread) ----
    let settings = ffi::Settings::new().expect("new_fluid_settings returned null");
    settings.set_num("synth.sample-rate", RATE as f64);
    // FluidSynth's default master gain is 0.2; we run well below that —
    // per-listener loudness is the CX1970 / PipeWire stream volume's job.
    settings.set_num("synth.gain", 0.05);
    let synth = ffi::Synth::new(&settings).expect("new_fluid_synth returned null");

    match synth.load_soundfont(SF2_PATH) {
        Ok(id) => println!("[init] soundfont loaded (id {id}): {SF2_PATH}"),
        Err(e) => {
            eprintln!("[init] {e}");
            std::process::exit(1);
        }
    }
    // ---- boot project (placeholder until project loading exists) ---------
    let mut project = Project::default_project();
    if let Err(e) = project.validate() {
        eprintln!("[init] internal default project invalid: {e}");
        std::process::exit(1);
    }
    // Demo content so pads 1–2 have audible scenes out of the box.
    let bass_eighths: [(usize, u8, u8); 8] = [
        (0, 36, 100),
        (2, 36, 80),
        (4, 36, 100),
        (6, 36, 80),
        (8, 36, 100),
        (10, 36, 80),
        (12, 36, 100),
        (14, 36, 80),
    ]; // C2 on every eighth
    project.tracks[0].clips[0] = Some(demo_clip(
        "Piano I",
        &[(0, 60, 90), (4, 60, 80), (8, 60, 90), (12, 60, 80)], // C4 quarters
    ));
    project.tracks[3].clips[0] = Some(demo_clip("Bass I", &bass_eighths));
    project.tracks[1].clips[1] = Some(demo_clip(
        "E.Piano I",
        &[(0, 60, 90), (4, 64, 90), (8, 67, 90), (12, 72, 90)], // C-E-G-C quarters
    ));
    project.tracks[3].clips[1] = Some(demo_clip("Bass II", &bass_eighths));
    println!(
        "[init] boot project: {} bpm, {} tracks — scenes 1-2 carry demo content",
        project.bpm,
        project.tracks.len()
    );

    let (tx, rx) = mpsc::channel::<TimedEvent>();
    let stats = Arc::new(Stats::default());

    // ---- audio thread -------------------------------------------------------
    let a_stats = Arc::clone(&stats);
    let _audio_thread = thread::Builder::new()
        .name("ps-audio".into())
        .spawn(move || {
            try_realtime_priority();

            // Fail the whole process, not just this thread: with no audio
            // thread the rest of the binary would feed a dead synth forever.
            let pcm = match open_playback(&device) {
                Ok(pcm) => pcm,
                Err(e) => {
                    eprintln!("could not open ALSA playback device '{device}': {e}");
                    std::process::exit(1);
                }
            };
            let io = pcm
                .io_checked::<i16>()
                .expect("could not create PCM IO handle");

            let mut left = vec![0f32; PERIOD_FRAMES];
            let mut right = vec![0f32; PERIOD_FRAMES];
            let mut out = vec![0i16; PERIOD_FRAMES * 2]; // interleaved

            // Track programs (D3: track N → internal channel N). Keys on
            // channel 0 thus play track 1's instrument.
            for (t, track) in project.tracks.iter().enumerate() {
                synth.program_change(t as i32, track.instrument.gm_program as i32);
            }

            let bpm = project.bpm;
            let mut cycle: u64 = 0;
            let mut total_samples: u64 = 0; // frames rendered so far (master clock domain)
            let mut playing_since: Option<u64> = None; // sample index where Play fired
            let mut active_scene: Option<usize> = None;
            let mut pending_launch: Option<(usize, Quantize)> = None; // (scene, quantize)
            loop {
                // 1) Drain pending MIDI events (bounded per cycle — writes stay timely).
                for _ in 0..64 {
                    match rx.try_recv() {
                        Ok(te) => {
                            let q_us = te.sent.elapsed().as_micros() as u64;
                            let feed_us = te.intended.elapsed().as_micros() as u64 - q_us;
                            a_stats.events.fetch_add(1, Ordering::Relaxed);
                            a_stats.q_sum_us.fetch_add(q_us, Ordering::Relaxed);
                            a_stats.q_max_us.fetch_max(q_us, Ordering::Relaxed);
                            a_stats.feed_max_us.fetch_max(feed_us, Ordering::Relaxed);
                            match te.event {
                                Event::NoteOn { chan, key, vel } => synth.note_on(chan, key, vel),
                                Event::NoteOff { chan, key } => synth.note_off(chan, key),
                                Event::Play => {
                                    if playing_since.is_none() {
                                        playing_since = Some(total_samples);
                                    }
                                }
                                Event::Stop => {
                                    playing_since = None;
                                    active_scene = None;
                                    pending_launch = None;
                                    a_stats.active_scene.store(0, Ordering::Relaxed);
                                    for t in 0..project.tracks.len() as i32 {
                                        synth.all_notes_off(t);
                                    }
                                }
                                Event::LaunchScene { index } => {
                                    let q =
                                        project.scenes[index].quantize.unwrap_or(Quantize::NextBar);
                                    if playing_since.is_none() {
                                        // Launching from stopped starts playback now.
                                        playing_since = Some(total_samples);
                                        active_scene = Some(index);
                                        a_stats
                                            .active_scene
                                            .store(index as u8 + 1, Ordering::Relaxed);
                                        for (t, track) in project.tracks.iter().enumerate() {
                                            synth.all_notes_off(t as i32);
                                            if track.clips[index].is_some() {
                                                synth.program_change(
                                                    t as i32,
                                                    track.instrument.gm_program as i32,
                                                );
                                            }
                                        }
                                    } else if q == Quantize::Immediate {
                                        active_scene = Some(index);
                                        a_stats
                                            .active_scene
                                            .store(index as u8 + 1, Ordering::Relaxed);
                                        for (t, track) in project.tracks.iter().enumerate() {
                                            synth.all_notes_off(t as i32);
                                            if track.clips[index].is_some() {
                                                synth.program_change(
                                                    t as i32,
                                                    track.instrument.gm_program as i32,
                                                );
                                            }
                                        }
                                    } else {
                                        pending_launch = Some((index, q));
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }

                // 1b) Master clock: fire beat ticks for every beat boundary the
                // playhead reaches inside this period (bounded — a 256-frame
                // period spans ~1% of a beat at 120 BPM; the loop runs ≤1x).
                match playing_since {
                    Some(start) => {
                        let b0 = clock::beats_elapsed(total_samples - start, RATE, bpm);
                        let b1 = clock::beats_elapsed(
                            total_samples + PERIOD_FRAMES as u64 - start,
                            RATE,
                            bpm,
                        );
                        let mut p = b0.ceil() as u64; // first beat position reached this period
                        while (p as f64) < b1 {
                            // Apply a pending scene launch at its quantized boundary.
                            if let Some((i, q)) = pending_launch
                                && match q {
                                    Quantize::NextBar => p.is_multiple_of(4),
                                    _ => true,
                                }
                            {
                                pending_launch = None;
                                active_scene = Some(i);
                                a_stats.active_scene.store(i as u8 + 1, Ordering::Relaxed);
                                for (t, track) in project.tracks.iter().enumerate() {
                                    synth.all_notes_off(t as i32);
                                    if track.clips[i].is_some() {
                                        synth.program_change(
                                            t as i32,
                                            track.instrument.gm_program as i32,
                                        );
                                    }
                                }
                            }
                            // Accent the downbeat; the kick self-decays (on+off now).
                            let vel: i32 = if p.is_multiple_of(4) { 110 } else { 70 };
                            synth.note_on(DRUM_CH, 36, vel);
                            synth.note_off(DRUM_CH, 36);
                            p += 1;
                        }

                        // Pattern playback: 16th-gated step notes for the active scene.
                        if let Some(scene) = active_scene {
                            let s0 = clock::steps_elapsed(total_samples - start, RATE, bpm);
                            let s1 = clock::steps_elapsed(
                                total_samples + PERIOD_FRAMES as u64 - start,
                                RATE,
                                bpm,
                            );
                            let mut k = s0.ceil() as u64; // first step reached this period
                            while (k as f64) < s1 {
                                for (t, track) in project.tracks.iter().enumerate() {
                                    if let Some(clip) = &track.clips[scene] {
                                        let len = clip.pattern.steps.len() as u64;
                                        // 16th gating: off the previous step's note,
                                        // on this one (sustains = repeated pitches).
                                        if let Some(n) =
                                            clip.pattern.steps[((k - 1) % len) as usize]
                                        {
                                            synth.note_off(t as i32, n.pitch as i32);
                                        }
                                        if let Some(n) = clip.pattern.steps[(k % len) as usize] {
                                            synth.note_on(
                                                t as i32,
                                                n.pitch as i32,
                                                n.velocity as i32,
                                            );
                                        }
                                    }
                                }
                                k += 1;
                            }
                        }

                        a_stats.playing.store(true, Ordering::Relaxed);
                        a_stats
                            .pos_beats_x1000
                            .store((b1 * 1000.0) as u32, Ordering::Relaxed);
                    }
                    None => {
                        a_stats.playing.store(false, Ordering::Relaxed);
                        a_stats.pos_beats_x1000.store(0, Ordering::Relaxed);
                    }
                }

                // 2) Render float L/R -> interleave + soft-clip to i16.
                let t0 = Instant::now();
                synth.render(PERIOD_FRAMES, &mut left, &mut right);
                for i in 0..PERIOD_FRAMES {
                    let l = (left[i] * i32::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
                    let r = (right[i] * i32::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
                    out[2 * i] = l as i16;
                    out[2 * i + 1] = r as i16;
                }
                let busy = t0.elapsed().as_nanos() as u64;

                // 3) Blocking write; recover from underruns.
                if let Err(e) = io.writei(&out) {
                    a_stats.xruns.fetch_add(1, Ordering::Relaxed);
                    if let Err(re) = pcm.recover(e.errno(), true) {
                        eprintln!(
                            "[audio] unrecoverable ALSA error: {re} (alsa errno {})",
                            e.errno()
                        );
                        return;
                    }
                }

                total_samples += PERIOD_FRAMES as u64;
                cycle += 1;
                a_stats.cycles.fetch_add(1, Ordering::Relaxed);
                a_stats.busy_ns.fetch_add(busy, Ordering::Relaxed);

                // 4) Sample the output queue depth occasionally (output-side latency).
                if cycle.is_multiple_of(64)
                    && let Ok(delay) = pcm.delay()
                {
                    a_stats
                        .delay_frames_max
                        .fetch_max(delay as u64, Ordering::Relaxed);
                }
            }
            // Note: `synth` then `settings` drop here (reverse declaration order), which is the correct teardown order.
        })
        .expect("failed to spawn audio thread");

    // ---- stats printer ------------------------------------------------------
    let s_stats = Arc::clone(&stats);
    thread::Builder::new()
        .name("ps-stats".into())
        .spawn(move || {
            let window = Duration::from_millis(2000);
            loop {
                thread::sleep(window);
                let [
                    cycles,
                    busy_ns,
                    xruns,
                    events,
                    q_sum,
                    q_max,
                    feed_max,
                    delay_max,
                ] = s_stats.snapshot_and_reset();
                let busy_pct = busy_ns as f64 / (window.as_nanos() as f64) * 100.0;
                let q_avg = if events > 0 {
                    q_sum as f64 / events as f64
                } else {
                    0.0
                };
                // Period cadence: how much of the ideal cycle budget (busy vs period duration).
                let period_us = PERIOD_FRAMES as f64 * 1e6 / RATE as f64;
                let playing = s_stats.playing.load(Ordering::Relaxed);
                let (bar, beat) = clock::bar_beat(
                    s_stats.pos_beats_x1000.load(Ordering::Relaxed) as f64 / 1000.0,
                );
                let scene = s_stats.active_scene.load(Ordering::Relaxed);
                let scene_str = if scene == 0 {
                    "-".to_string()
                } else {
                    format!("S{scene}")
                };
                println!(
                    "[stats] cycles={cycles:5} busy={busy_pct:4.2}% ({:.1} us/{} us period) \
                     events={events:3} queue avg/max={q_avg:6.1}/{q_max:6} us \
                     feeder_jitter_max={feed_max:5} us xruns={xruns} out_queue~{:.1} ms \
                     transport={} pos={bar}.{beat} scene={}",
                    if cycles > 0 {
                        busy_ns as f64 / cycles as f64 / 1000.0
                    } else {
                        0.0
                    },
                    period_us,
                    delay_max as f64 * 1000.0 / RATE as f64,
                    if playing { "playing" } else { "stopped" },
                    scene_str,
                );
            }
        })
        .expect("failed to spawn stats thread");

    // ---- event source: live MIDI (default) or scripted demo (--demo) --------
    if demo {
        run_demo_feeder(tx);
    } else {
        run_live_midi(tx);
    }
}

/// Scripted note feeder, no MIDI hardware required (~2-bar arpeggio loop at
/// 120 BPM; beat ticks come from the master clock). Kept so the audio path
/// stays testable without the unit plugged in.
fn run_demo_feeder(tx: mpsc::Sender<TimedEvent>) {
    println!("[feeder] demo arpeggio @ 120 BPM — Ctrl+C to stop");
    let pattern: [i32; 8] = [60, 64, 67, 71, 72, 71, 67, 64]; // C maj arpeggio, eighth notes
    let step = Duration::from_millis(250);
    let start = Instant::now();
    // Stamp `sent` at enqueue time so engine-side queue latency is separable from feeder wake-up jitter.
    let send = |intended: Instant, event: Event| {
        let _ = tx.send(TimedEvent {
            intended,
            sent: Instant::now(),
            event,
        });
    };

    // Launch scene 1 from stopped: starts the master clock AND activates the
    // demo patterns, so --demo exercises the full pipeline without hardware.
    send(start, Event::LaunchScene { index: 0 });

    let mut i: u32 = 0;
    loop {
        let t = start + step * i;
        if let Some(remaining) = t.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        }

        // Lead: note on now, note off for the *previous* step's note.
        let key = pattern[(i % pattern.len() as u32) as usize];
        send(
            t,
            Event::NoteOn {
                chan: LEAD_CH,
                key,
                vel: 96,
            },
        );
        if i > 0 {
            let prev = pattern[((i - 1) % pattern.len() as u32) as usize];
            send(
                t,
                Event::NoteOff {
                    chan: LEAD_CH,
                    key: prev,
                },
            );
        }

        // (Drum ticks + scene patterns are the audio loop's job now.)

        i += 1;
    }
}

/// Live MIDI source: `spawn_pump` → per-port Oxygen mappers → engine events.
///
/// One mapper per source port (the HUI subdevice is the only place
/// loop/record are distinguishable — probe A2). Runs on the main thread; the
/// audio thread stays untouched. Keys drive track 1's voice, pads 1–8 launch
/// scenes, transport drives the master clock.
fn run_live_midi(tx: mpsc::Sender<TimedEvent>) {
    for m in [default_mapping(), hui_mapping()] {
        if let Err(e) = m.validate() {
            eprintln!("internal default mapping invalid: {e}");
            std::process::exit(1);
        }
    }
    let mut music = Mapper::new(default_mapping());
    let mut hui = Mapper::new(hui_mapping());

    let (mtx, mrx) = mpsc::channel::<PumpMessage>();
    // Keep the handle alive for as long as we consume — dropping it shuts the
    // pump down (its Drop impl signals + joins).
    let _pump = match spawn_pump(mtx, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot start MIDI pump: {e}");
            std::process::exit(1);
        }
    };

    println!("[midi] live — put the unit in preset mode (DAW LED unlit); Ctrl+C to stop");
    loop {
        match mrx.recv() {
            Ok(PumpMessage::Midi(m)) => {
                let mut out = Vec::new();
                if m.port.ends_with(",1") {
                    hui.map(&m.event, &mut out);
                } else {
                    music.map(&m.event, &mut out);
                }
                for action in out {
                    match action {
                        Action::Key { note, on, velocity } => {
                            let event = if on {
                                Event::NoteOn {
                                    chan: LEAD_CH,
                                    key: note as i32,
                                    vel: velocity as i32,
                                }
                            } else {
                                Event::NoteOff {
                                    chan: LEAD_CH,
                                    key: note as i32,
                                }
                            };
                            send_event(&tx, m.received, event);
                        }
                        // Pads 1–8 launch scenes (R2: pad N = scene N).
                        Action::Pad { index, on, .. } if on && (1..=8).contains(&index) => {
                            send_event(
                                &tx,
                                m.received,
                                Event::LaunchScene {
                                    index: (index - 1) as usize,
                                },
                            );
                        }
                        Action::Play => {
                            println!("[transport] play");
                            send_event(&tx, m.received, Event::Play);
                        }
                        Action::Stop => {
                            println!("[transport] stop");
                            send_event(&tx, m.received, Event::Stop);
                        }
                        Action::LoopToggle => {
                            println!("[transport] loop toggle (flag only — no behavior yet)")
                        }
                        Action::Record => {
                            println!("[transport] record (no-op — MIDI capture is a later slice)")
                        }
                        _ => {} // faders/knobs/mod/pitch: no engine effect yet
                    }
                }
            }
            Ok(PumpMessage::PortConnected(p)) => println!("[midi] [+] {p}"),
            Ok(PumpMessage::PortDisconnected(p)) => println!("[midi] [-] {p}"),
            Err(_) => break, // pump gone — shut down
        }
    }
}

/// Sparse one-bar pattern from `(step, pitch, velocity)` entries (demo content).
fn demo_clip(name: &str, entries: &[(usize, u8, u8)]) -> Clip {
    let mut steps = vec![None; STEPS_PER_BAR];
    for (i, pitch, vel) in entries {
        steps[*i] = Some(StepNote {
            pitch: *pitch,
            velocity: *vel,
        });
    }
    Clip {
        name: name.into(),
        pattern: Pattern { bars: 1, steps },
    }
}

/// Enqueue an event for the audio thread, stamping `sent` at enqueue time so
/// queue latency stays separable from source-side jitter.
fn send_event(tx: &mpsc::Sender<TimedEvent>, received: Instant, event: Event) {
    let _ = tx.send(TimedEvent {
        intended: received,
        sent: Instant::now(),
        event,
    });
}

// ------------------------------------------------------------------ helpers

/// Opens ALSA playback (`default` = PipeWire ALSA plugin on the dev box; plain
/// ALSA/dmix or `plughw:` elsewhere) with our fixed geometry.
fn open_playback(device: &str) -> alsa::Result<PCM> {
    let pcm = PCM::new(device, Direction::Playback, false)?;

    let (actual_period, buf, actual_rate);
    {
        // NB: HwParams::any (fresh params), NOT hw_params_current — the latter
        // fails with EBADMSG(77) on PipeWire's ALSA plugin.
        let h = alsa::pcm::HwParams::any(&pcm)?;
        h.set_access(Access::RWInterleaved)?;
        h.set_format(Format::S16LE)?;
        h.set_rate(RATE, ValueOr::Nearest)?;
        h.set_channels(2)?;
        h.set_periods(N_PERIODS, ValueOr::Nearest)?;
        h.set_period_size(PERIOD_FRAMES as Frames, ValueOr::Nearest)?;
        pcm.hw_params(&h)?;

        actual_period = h.get_period_size().unwrap_or(PERIOD_FRAMES as Frames);
        buf = h.get_buffer_size()?;
        actual_rate = h.get_rate()?;

        // Start as soon as one period is queued (lowest startup latency).
        // Best-effort: default (start when buffer full) still works, just starts ~16 ms later.
        if let Ok(sw) = pcm.sw_params_current()
            && sw.set_start_threshold(actual_period).is_ok()
        {
            let _ = pcm.sw_params(&sw);
        }
    } // h/sw guards dropped here so `pcm` can be moved out

    println!(
        "[alsa] opened '{device}': rate={actual_rate} period={actual_period} frames buffer={buf} frames"
    );
    Ok(pcm)
}

/// Attempts SCHED_FIFO on the calling (audio) thread. Logs pass/fail — on a
/// stock desktop this needs privileges; the Pi setup will grant it via
/// limits/systemd. Not fatal either way for the spike.
fn try_realtime_priority() {
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
    }
    unsafe extern "C" {
        fn sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> i32;
    }
    const SCHED_FIFO: i32 = 1; // libc::SCHED_FIFO
    let param = SchedParam { sched_priority: 80 };
    let rc = unsafe { sched_setscheduler(0, SCHED_FIFO, &param) };
    if rc == 0 {
        println!("[rt] SCHED_FIFO priority 80 acquired");
    } else {
        let err = std::io::Error::last_os_error();
        println!(
            "[rt] SCHED_FIFO unavailable ({err}) — running with default scheduler (fine for spike)"
        );
    }
}
