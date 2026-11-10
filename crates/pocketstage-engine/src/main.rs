//! PocketStage — audio engine (spike validated; now wired to real MIDI).
//!
//! Architecture (scratchpad R5):
//!   * One FluidSynth instance, owned exclusively by the audio thread.
//!   * Single writer: everything else sends `Event`s across an mpsc channel;
//!     no locks, no allocation on the render path.
//!   * ALSA S16LE stereo, 256-frame periods x 3 @ 48 kHz, underrun recovery.
//!
//! The per-period *decision* logic (events, transport, recording, takes,
//! swaps) lives in `core.rs` behind a [`SynthBackend`] trait — that's the
//! device-free integration harness (backlog 13). This file keeps only what
//! needs the platform: ALSA open/write/recover, FluidSynth boot setup, the
//! MIDI pump feeder, stats printing, and the embedded HTTP server spawn.
//!
//! Event sources:
//!   * default: live MIDI — `spawn_pump` → per-port Oxygen mappers (music +
//!     HUI) → engine events. Keys drive track 1's voice; pads 1–8 launch
//!     scenes or hit the drum kit depending on unit mode (D11–D13).
//!   * `--demo`: scripted arpeggio feeder, no hardware needed (spike mode).
//!
//! Run:  cargo run --release -p pocketstage-engine            (live MIDI)
//!       cargo run --release -p pocketstage-engine -- --demo  (no hardware)
//! ALSA device: positional arg > `POCKETSTAGE_DEVICE` env var > `default`.

mod clock;
mod core;
mod ffi;
mod transport;

use alsa::pcm::{Access, Format, Frames, PCM};
use alsa::{Direction, ValueOr};
use pocketstage_midi::{PumpMessage, spawn_pump};
use pocketstage_model::{
    Clip, Pattern, ProjectFile, SCHEMA_VERSION, STEPS_PER_BAR, StepNote, Take,
};
use pocketstage_oxygen::{Action, Mapper, Mode, default_mapping, hui_mapping};
use pocketstage_server::{AppState, EngineCommand, LiveStatus, ProjectStore};

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use core::{EngineCore, Event, FluidBackend, LEAD_CH, PERIOD_FRAMES, RATE, Stats, TimedEvent};

const N_PERIODS: u32 = 3;
const SF2_PATH: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

/// Curated patch browser for DAW mode (D13): slot = pitch % 12. Letter B
/// (slot 11) is the settings key, so there are eleven patches.
const PATCHES: [(u8, &str); 11] = [
    (0, "Grand Piano"),
    (4, "Electric Piano"),
    (6, "Harpsichord"),
    (11, "Vibraphone"),
    (19, "Church Organ"),
    (24, "Nylon Guitar"),
    (29, "Overdrive Guitar"),
    (32, "Acoustic Bass"),
    (38, "Synth Bass 1"),
    (48, "Strings"),
    (65, "Alto Sax"),
];
/// Letter B → loop-length settings key instead of a patch slot.
const SETTINGS_KEY_SLOT: u8 = 11;

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
    // FluidSynth's default master gain is 0.2. The old 0.05 was a workaround
    // for the float->i16 clipping bug (fixed in core::to_i16); with the correct
    // conversion, 0.3 puts a single hard note around -19 dBFS and an 8-note
    // max-velocity chord around -8 dBFS — audible with headroom left before
    // true full scale. Per-listener loudness is still PipeWire's job.
    settings.set_num("synth.gain", 0.3);
    // FX policy (session-10 "held keys sound stacked" post-mortem, see
    // examples/fx_bench.rs): boot with FluidSynth's BUILT-IN effect config
    // and never write space/rate/depth params. A project only rides its
    // fx_reverb / fx_chorus knobs (levels); unset = touch nothing.
    let synth = ffi::Synth::new(&settings).expect("new_fluid_synth returned null");

    match synth.load_soundfont(SF2_PATH) {
        Ok(id) => println!("[init] soundfont loaded (id {id}): {SF2_PATH}"),
        Err(e) => {
            eprintln!("[init] {e}");
            std::process::exit(1);
        }
    }
    // Pads + metronome live on channels 8..=15; make them percussion. Note:
    // set_channel_type alone does NOT re-resolve the channel's preset — the
    // bank_select + program_change pair is what actually lands them on the
    // GM drum kit (lab: examples/drum_chan_lab.rs).
    for ch in core::TICK_CH..=15 {
        synth.set_channel_type_drum(ch);
        synth.bank_select(ch, 128); // GM percussion bank
        synth.program_change(ch, 0);
    }
    // ---- boot project: load from the store (single source of truth) ------
    // Same store the HTTP control plane reads/writes; PUTs hot-swap the live
    // project via EngineCommand. Precedence: POCKETSTAGE_PROJECT = single
    // file (tests/CI); otherwise a switchable library in ~/.pocketstage/projects
    // (the legacy project.json is migrated in on first launch).
    let store = Arc::new(
        match std::env::var("POCKETSTAGE_PROJECT") {
            Ok(project_path) => ProjectStore::open_or_default(&project_path)
                .inspect(|s| println!("[init] single project: {}", s.path().display()))
                .map_err(|e| (project_path, e)),
            Err(_) => {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                let dir = format!("{home}/.pocketstage/projects");
                ProjectStore::open_library(&dir)
                    .inspect(|s| println!("[init] library {dir}: current '{}'", s.current_name()))
                    .map_err(|e| (dir, e))
            }
        }
        .unwrap_or_else(|(p, e)| {
            eprintln!("[init] cannot open project store '{p}': {e}");
            std::process::exit(1);
        }),
    );
    let fresh_project = !store.file_exists();
    let mut project = store.get().project;
    if let Err(e) = project.validate() {
        eprintln!("[init] internal default project invalid: {e}");
        std::process::exit(1);
    }
    // Demo content so pads 1–2 have audible scenes — only for a brand-new
    // project, never stamped over a user's saved work.
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
    if fresh_project && project.tracks.len() >= 4 {
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
        // Persist immediately: the store stays the single source of truth,
        // so GET /api/project matches what is live from t=0.
        if let Err(e) = store.replace(ProjectFile {
            version: SCHEMA_VERSION,
            project: project.clone(),
        }) {
            eprintln!("[init] warning: could not seed demo project: {e}");
        }
    } else if !project.tracks.is_empty() {
        println!("[init] loaded saved project (no demo stamping)");
    }
    println!(
        "[init] boot project '{}' from {}: {} bpm, {} tracks",
        project.name,
        store.path().display(),
        project.bpm,
        project.tracks.len()
    );

    let (tx, rx) = mpsc::channel::<TimedEvent>();
    // HTTP → audio-thread command lane (single writer: the server only ever
    // *requests* state changes; the drain loop applies them — R5).
    let (ctx, crx) = mpsc::channel::<EngineCommand>();
    // Finalized takes flow back the other way: audio → HTTP layer, which
    // appends them to the saved project so recordings survive reboot.
    let (take_feed, take_rx) = mpsc::channel::<Take>();
    // D24 write-through lane: engine-originated project edits (MIX/SEQ/B-key)
    // go out as debounced full snapshots for persistence — outbound only.
    let (edit_feed, edit_rx) = mpsc::channel::<ProjectFile>();
    let live = Arc::new(LiveStatus::default());
    let stats = Arc::new(Stats {
        live: Arc::clone(&live),
        ..Default::default()
    });

    // ---- embedded HTTP control plane (own thread + tokio runtime) ---------
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7777);
    let http_state =
        AppState::embedded(Arc::clone(&store), ctx, Arc::clone(&live), take_rx, edit_rx);
    match pocketstage_server::spawn_engine_server(http_state, ([0, 0, 0, 0], port).into()) {
        Ok(()) => println!("[server] control plane on http://0.0.0.0:{port} (LAN-trusted)"),
        Err(e) => eprintln!("[server] NOT started ({e}) — audio unaffected"),
    }

    // ---- audio thread -------------------------------------------------------
    let a_stats = Arc::clone(&stats);
    let audio_thread = thread::Builder::new()
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

            // The core owns all per-period state; the fake-backend test twin
            // lives in core.rs harness_tests (device-free CI).
            let mut core = EngineCore::new(
                project,
                Arc::clone(&live),
                Arc::clone(&a_stats),
                take_feed,
                edit_feed,
            );
            let mut backend = FluidBackend(&synth);
            // Track programs + mixer (D3: track N → internal channel N).
            // Keys on channel 0 thus play track 1's instrument (and level).
            core.apply_all_voices(&mut backend);

            loop {
                // One cycle: commands → events → clock → render (all in core).
                core.period(&mut backend, &crx, &rx);

                // Blocking write of the rendered period; recover from underruns.
                if let Err(e) = io.writei(core.output()) {
                    a_stats.xruns.fetch_add(1, Ordering::Relaxed);
                    if let Err(re) = pcm.recover(e.errno(), true) {
                        eprintln!(
                            "[audio] unrecoverable ALSA error: {re} (alsa errno {})",
                            e.errno()
                        );
                        return;
                    }
                }

                // Sample the output queue depth occasionally (output-side latency).
                if core.cycles.is_multiple_of(64)
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
            // Watchdog: a panic in ps-audio silences every surface while the
            // rest of the process keeps running — make that state obvious.
            let audio_watchdog = audio_thread;
            let mut audio_dead_reported = false;
            let window = Duration::from_millis(2000);
            loop {
                thread::sleep(window);
                if !audio_dead_reported && audio_watchdog.is_finished() {
                    audio_dead_reported = true;
                    eprintln!(
                        "[FATAL] ps-audio thread died (see panic above) — \
                               NO AUDIO until restart. RUST_BACKTRACE=1 for detail."
                    );
                }
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
                let playing = s_stats.live.playing.load(Ordering::Relaxed);
                let (bar, beat) = clock::bar_beat(
                    s_stats.live.pos_beats_x1000.load(Ordering::Relaxed) as f64 / 1000.0,
                );
                let scene = s_stats.live.active_scene.load(Ordering::Relaxed);
                let scene_str = if scene == 0 {
                    "-".to_string()
                } else {
                    format!("S{scene}")
                };
                let recording = s_stats.live.recording.load(Ordering::Relaxed);
                println!(
                    "[stats] cycles={cycles:5} busy={busy_pct:4.2}% ({:.1} us/{} us period) \
                     events={events:3} queue avg/max={q_avg:6.1}/{q_max:6} us \
                     feeder_jitter_max={feed_max:5} us xruns={xruns} out_queue~{:.1} ms \
                     transport={} pos={bar}.{beat} scene={} takes={} swaps={}",
                    if cycles > 0 {
                        busy_ns as f64 / cycles as f64 / 1000.0
                    } else {
                        0.0
                    },
                    period_us,
                    delay_max as f64 * 1000.0 / RATE as f64,
                    if recording {
                        "recording"
                    } else if playing {
                        "playing"
                    } else {
                        "stopped"
                    },
                    scene_str,
                    s_stats.live.takes.load(Ordering::Relaxed),
                    s_stats.project_updates.load(Ordering::Relaxed),
                );
            }
        })
        .expect("failed to spawn stats thread");

    // ---- event source: live MIDI (default) or scripted demo (--demo) --------
    if demo {
        run_demo_feeder(tx);
    } else {
        run_live_midi(tx, Arc::clone(&stats));
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

        // Scripted recording session (hardware-free E2E for D16): record a
        // slice of the arpeggio, stop, then play it back looping under the
        // still-running feeder.
        match i {
            16 => send(t, Event::StartRecording), // 4 s in: ⏺ (metronome on)
            48 => send(t, Event::Stop),           // 12 s: finalize take, silent
            52 => send(t, Event::Play),           // 13 s: take loops (no ticks)
            _ => {}
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
fn run_live_midi(tx: mpsc::Sender<TimedEvent>, stats: Arc<Stats>) {
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

    // D11: PRESET = perform (keys + drum pads), DAW = manage (scenes + patch
    // browser). Boot assumption; self-heals on the first DAW/PRESET press.
    let mut mode = Mode::Preset;
    println!("[midi] live — PRESET perform / DAW manage (D11); Ctrl+C to stop");
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
                        // Keys: PRESET plays the perform voice; DAW turns the
                        // whole keyboard into a patch browser (D13) — every
                        // note selects via pitch % 12 and auditions it live.
                        Action::Key { note, on, velocity } => {
                            if on && mode == Mode::Daw {
                                let slot = note % 12;
                                if slot == SETTINGS_KEY_SLOT {
                                    println!("[daw] B — cycle loop length");
                                    send_event(&tx, m.received, Event::CycleLoopBars);
                                } else {
                                    let (prog, name) = PATCHES[slot as usize];
                                    println!("[daw] select #{slot} {name} (gm {prog})");
                                    send_event(
                                        &tx,
                                        m.received,
                                        Event::SetLeadProgram { program: prog },
                                    );
                                }
                            }
                            // Notes always sound (perform in PRESET, audition
                            // in DAW); the keyboard's own note-off releases.
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
                        // Pads: PRESET = fixed drum kit (D12); DAW = scene
                        // launching (R2: pad N = scene N).
                        Action::Pad {
                            index,
                            on,
                            velocity,
                        } if (1..=8).contains(&index) => match mode {
                            Mode::Preset => {
                                // Live kit (project.drum_kit or the engine
                                // default), read lock-free from the atomic
                                // the audio thread keeps current.
                                let kit =
                                    core::kit_or_default(stats.drum_kit.load(Ordering::Relaxed));
                                let key = kit[(index - 1) as usize] as i32;
                                // One channel per pad so retriggers kill only this pad.
                                let chan = core::PAD_CH_BASE + (index - 1) as i32;
                                let event = if on {
                                    Event::NoteOn {
                                        chan,
                                        key,
                                        vel: velocity.max(1) as i32,
                                    }
                                } else {
                                    Event::NoteOff { chan, key }
                                };
                                send_event(&tx, m.received, event);
                            }
                            Mode::Daw => {
                                if on {
                                    send_event(
                                        &tx,
                                        m.received,
                                        Event::LaunchScene {
                                            index: (index - 1) as usize,
                                        },
                                    );
                                }
                            }
                        },
                        Action::ModeChanged(new_mode) => {
                            if new_mode != mode {
                                mode = new_mode;
                                stats
                                    .live
                                    .daw_mode
                                    .store(new_mode == Mode::Daw, Ordering::Relaxed);
                                println!(
                                    "[mode] {}",
                                    match new_mode {
                                        Mode::Daw => "DAW — manage (pads=scenes, keys=patches)",
                                        Mode::Preset => "PRESET — perform (keys+drums)",
                                    }
                                );
                                if new_mode == Mode::Daw {
                                    // D15: recording stops here; playback never does.
                                    send_event(&tx, m.received, Event::StopRecording);
                                }
                            }
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
                            println!("[transport] record");
                            send_event(&tx, m.received, Event::StartRecording);
                        }
                        _ => {} // faders/knobs/mod/pitch/pads 9–16: no engine effect yet
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
