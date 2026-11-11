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
    ActiveVoice, Clip, Pattern, ProjectFile, SCHEMA_VERSION, STEPS_PER_BAR, StepNote, Take,
};
use pocketstage_oxygen::{Action, Mapper, Mode, default_mapping, hui_mapping};
use pocketstage_server::{AppState, EngineCommand, LiveStatus, ProjectStore, VOICE_DRUMS};

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use core::{EngineCore, Event, FluidBackend, LEAD_CH, PERIOD_FRAMES, RATE, Stats, TimedEvent};

const N_PERIODS: u32 = 3;
const SF2_PATH: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

/// D25 white-key patch browser (replaces D13's pitch%12 slots). Slot 0 is
/// the lowest white key (C2) = the kit; slots 1..=11 walk up the white keys
/// (D2 E2 F2 G2 A2 B2 C3 D3 E3 F3 G3) with the curated GM patches. In DAW
/// mode a white-key press selects + auditions; black keys are inert.
const VOICES: [(ActiveVoice, &str); 12] = [
    (ActiveVoice::Drums, "Drums"),
    (ActiveVoice::Program(0), "Grand Piano"),
    (ActiveVoice::Program(4), "Electric Piano"),
    (ActiveVoice::Program(6), "Harpsichord"),
    (ActiveVoice::Program(11), "Vibraphone"),
    (ActiveVoice::Program(19), "Church Organ"),
    (ActiveVoice::Program(24), "Nylon Guitar"),
    (ActiveVoice::Program(29), "Overdrive Guitar"),
    (ActiveVoice::Program(32), "Acoustic Bass"),
    (ActiveVoice::Program(38), "Synth Bass 1"),
    (ActiveVoice::Program(48), "Strings"),
    (ActiveVoice::Program(65), "Alto Sax"),
];

/// Browser slot of a note: white keys count from the anchor C2 (36); black
/// keys and notes past the table return None (inert in the browser).
fn white_slot(note: u8) -> Option<usize> {
    const WHITE_PC: [bool; 12] = [
        true, false, true, false, true, true, false, true, false, true, false, true,
    ];
    if !(36..=84).contains(&note) || !WHITE_PC[(note % 12) as usize] {
        return None;
    }
    let idx = (36..=note).filter(|k| WHITE_PC[(*k % 12) as usize]).count() - 1;
    (idx < VOICES.len()).then_some(idx)
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
                let overlay = match s_stats.live.overlay.load(Ordering::Relaxed) {
                    1 => "MIX",
                    2 => "SEQ",
                    _ => "-",
                };
                let voice = s_stats.live.active_voice.load(Ordering::Relaxed);
                let voice_str = if voice == VOICE_DRUMS {
                    "drums".to_string()
                } else {
                    format!("gm{voice}")
                };
                println!(
                    "[stats] cycles={cycles:5} busy={busy_pct:4.2}% ({:.1} us/{} us period) \
                     events={events:3} queue avg/max={q_avg:6.1}/{q_max:6} us \
                     feeder_jitter_max={feed_max:5} us xruns={xruns} out_queue~{:.1} ms \
                     transport={} pos={bar}.{beat} scene={} takes={} swaps={} overlay={} voice={}",
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
                    overlay,
                    voice_str,
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
    // Fixed pad note base for melodic pad routing (D25 Q2).
    let pad_base = music.mapping().pad_note_base as i32;

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
                        // Keys: DAW = white-key voice browser (D25); PRESET
                        // plays the ONE selected voice — melodic programs on
                        // all 49 keys, Drums silences the keybed.
                        Action::Key { note, on, velocity } => {
                            if mode == Mode::Daw {
                                // White key press selects its voice; programs
                                // also audition (the note sounds); Drums is
                                // confirmed by the core's kick. Black keys
                                // are inert.
                                if let Some(slot) = white_slot(note) {
                                    let (voice, name) = VOICES[slot];
                                    if on {
                                        println!("[daw] select #{slot} {name}");
                                        send_event(&tx, m.received, Event::SelectVoice { voice });
                                    }
                                    if voice != ActiveVoice::Drums {
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
                                }
                            } else if stats.live.active_voice.load(Ordering::Relaxed) == VOICE_DRUMS
                            {
                                // DRUMS voice: the keybed is silent — pads
                                // are the instrument (D25).
                            } else {
                                // Melodic voice: keys play it — all 49 of
                                // them, no dead pad-shadow octave (Q2).
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
                        }
                        // PRESET pads (D25): instrument first — the kit
                        // across both banks in DRUMS voice, melody notes
                        // otherwise. Command roles only while an overlay is
                        // live (exit must stay reachable).
                        Action::Pad {
                            index,
                            on,
                            velocity,
                        } if mode == Mode::Preset => {
                            let ov = stats.live.overlay.load(Ordering::Relaxed);
                            let bank2 = (9..=16).contains(&index);
                            if ov != 0 && bank2 {
                                if on {
                                    match index {
                                        9 => send_event(&tx, m.received, Event::ToggleMixOverlay),
                                        10 => send_event(&tx, m.received, Event::ToggleSeqOverlay),
                                        11 if ov == 2 => {
                                            send_event(&tx, m.received, Event::SeqNextBar)
                                        }
                                        12 if ov == 2 => {
                                            send_event(&tx, m.received, Event::SeqFlipWindow)
                                        }
                                        _ => {}
                                    }
                                }
                            } else if ov == 2 && !bank2 {
                                // SEQ programmer on bank 1 (D23) — wins over
                                // any voice.
                                if on {
                                    send_event(&tx, m.received, Event::SeqStep { step: index });
                                }
                            } else if stats.live.active_voice.load(Ordering::Relaxed) == VOICE_DRUMS
                            {
                                // Kit spans both pad banks (Q3); channels
                                // mirror across banks (pad 9 shares pad 1's
                                // kill group — documented trade-off).
                                let packed = if bank2 {
                                    stats.drum_kit_2.load(Ordering::Relaxed)
                                } else {
                                    stats.drum_kit.load(Ordering::Relaxed)
                                };
                                let kit = core::kit_or_default(packed);
                                let slot = if bank2 { index - 9 } else { index - 1 };
                                let key = kit[slot as usize] as i32;
                                let chan = core::PAD_CH_BASE + slot as i32;
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
                            } else {
                                // Melodic voice: every pad press is a key of
                                // the voice — pads transmit plain notes and
                                // with one instrument there is nothing to
                                // disambiguate (D25 Q2).
                                let key = pad_base + index as i32 - 1;
                                let event = if on {
                                    Event::NoteOn {
                                        chan: LEAD_CH,
                                        key,
                                        vel: velocity.max(1) as i32,
                                    }
                                } else {
                                    Event::NoteOff { chan: LEAD_CH, key }
                                };
                                send_event(&tx, m.received, event);
                            }
                        }
                        // DAW pads 1–8 = scene launching (R2).
                        Action::Pad {
                            index: index @ 1..=8,
                            on: true,
                            ..
                        } if mode == Mode::Daw => {
                            send_event(
                                &tx,
                                m.received,
                                Event::LaunchScene {
                                    index: (index - 1) as usize,
                                },
                            );
                        }
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
                                        Mode::Daw => {
                                            "DAW — manage (pads=scenes, white keys=voices)"
                                        }
                                        Mode::Preset => {
                                            "PRESET — perform (one voice at a time)"
                                        }
                                    }
                                );
                                if new_mode == Mode::Daw {
                                    // D21/D25: the manage mode is a clean
                                    // slate — exit MIX/SEQ. Overlay ENTRY
                                    // lives on DAW pads 9/10 now; an overlay
                                    // armed there survives the trip back to
                                    // PRESET (that's the point).
                                    send_event(&tx, m.received, Event::ExitOverlays);
                                    // D15: recording stops here; playback never does.
                                    send_event(&tx, m.received, Event::StopRecording);
                                }
                            }
                        }
                        // DAW bank 2 pads 9/10 = overlay ENTRY (D25: PRESET
                        // bank 2 is instrument now, so entering MIX/SEQ
                        // belongs to the manage mode). Exit stays on PRESET
                        // bank 2 while an overlay lives.
                        Action::Pad {
                            index: index @ (9 | 10),
                            on: true,
                            ..
                        } if mode == Mode::Daw => {
                            if index == 9 {
                                send_event(&tx, m.received, Event::ToggleMixOverlay);
                            } else {
                                send_event(&tx, m.received, Event::ToggleSeqOverlay);
                            }
                        }
                        // Faders/knobs/fader-buttons: forwarded unconditionally —
                        // the core interprets them only while MIX is on (D22),
                        // and fader buttons are channel selectors, never scene
                        // triggers (D14).
                        Action::Fader { index, value } => {
                            send_event(&tx, m.received, Event::MixFader { index, value })
                        }
                        Action::Knob { index, value } => {
                            send_event(&tx, m.received, Event::MixKnob { index, value })
                        }
                        Action::FaderButton {
                            index,
                            pressed: true,
                        }
                        | Action::FaderButtonTap { index } => {
                            send_event(&tx, m.received, Event::SelectTrack { index })
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
                        _ => {} // mod wheel, pitch lever, pad note-offs: no engine effect yet
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
