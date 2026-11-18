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
//!     HUI) → engine events. One unified raw-note router (D25): pads are
//!     plain notes 36–51, no special lane; routing follows mode + voice.
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
use pocketstage_midi::{Event as MidiEvent, PumpMessage, spawn_pump};
use pocketstage_model::{
    ActiveVoice, Clip, Pattern, ProjectFile, SCHEMA_VERSION, STEPS_PER_BAR, StepNote, Take,
};
use pocketstage_oxygen::{
    Action, Command as KeymapCommand, DawKeymap, Mapper, Mode, Port as KeymapPort,
    Source as KeymapSource,
};
use pocketstage_server::{
    AppState, EngineCommand, KeymapSlot, LiveStatus, ProjectStore, VOICE_DRUMS,
};

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use core::{EngineCore, Event, FluidBackend, LEAD_CH, PERIOD_FRAMES, RATE, Stats, TimedEvent};

const N_PERIODS: u32 = 3;
const SF2_PATH: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

// D25's white-key browser table and the overlay command notes used to be
// hardcoded here. Keymap phase 3 (D26): they are DATA — `DawKeymap::default()`
// reproduces the old table exactly (regression-locked in the oxygen crate),
// and the feeder resolves every DAW-mode note through `route_daw_note`.

/// Per-message DAW-mode routing decision for one raw note. Pure + total —
/// table-tested without MIDI (phase-3 test-first rule).
#[derive(Debug, PartialEq, Eq)]
enum DawRoute {
    /// Voice binding: a press selects (voice-confirm sounds); the pressed
    /// pitch rides through for melodic voices (Q3 WYSIWYG audition).
    Voice { voice: ActiveVoice, label: String },
    /// Command binding: momentary, fires on note-on only.
    Command(KeymapCommand),
    /// Nothing bound — inert in DAW mode.
    Inert,
}

fn route_daw_note(km: &DawKeymap, note: u8, port: KeymapPort) -> DawRoute {
    // Validation guarantees a source is never bound twice, so the two
    // lookups cannot disagree.
    if let Some(b) = km.voice_for(note, port) {
        return DawRoute::Voice {
            voice: b.voice,
            label: b.label.clone(),
        };
    }
    if let Some(cmd) = km.command_for(KeymapSource::note(note), port) {
        return DawRoute::Command(cmd);
    }
    DawRoute::Inert
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

    let sfont_id = match synth.load_soundfont(SF2_PATH) {
        Ok(id) => {
            println!("[init] soundfont loaded (id {id}): {SF2_PATH}");
            id
        }
        Err(e) => {
            eprintln!("[init] {e}");
            std::process::exit(1);
        }
    };
    // Preset catalog for the web voice dropdown — read from the FONT, not a
    // hardcoded table. Must happen here on the main thread: FluidSynth's
    // iteration is not thread-safe and the synth moves to ps-audio next.
    let presets: Vec<pocketstage_server::PresetInfo> = synth
        .preset_list(sfont_id)
        .into_iter()
        .map(|(bank, program, name)| pocketstage_server::PresetInfo {
            bank,
            program,
            name,
        })
        .collect();
    println!("[init] preset catalog: {} entries", presets.len());
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
    // ---- DAW keymap (D26 phase 2): load once, share with the HTTP plane ---
    // NEVER fatal: missing file = defaults; corrupt file = defaults + loud
    // warning (the slot logs it). The MIDI feeder reads this slot; the
    // audio thread never does. Phase 3 wires routing to it.
    let keymap = Arc::new(pocketstage_server::KeymapSlot::open(
        pocketstage_server::keymap_path(),
    ));
    // ---- device mapping (phase 5, D29/D37): same never-fail-open loader ---
    // Measured factory defaults on missing/corrupt file (+ loud warn). One
    // Arc feeds BOTH the HTTP plane (GET/PUT/reset) and the MIDI feeder,
    // which hot-reloads its mappers when the published Arc changes.
    let mapping = Arc::new(pocketstage_server::MappingSlot::open(
        pocketstage_server::mapping_path(),
    ));
    println!(
        "[init] device mapping: {} (music ch {}, hui transport notes {})",
        mapping.path().display(),
        mapping.get().music.control_channel,
        if mapping.get().hui.transport_notes.is_some() {
            "on"
        } else {
            "off"
        },
    );
    println!(
        "[init] daw keymap: {} (schema {}, {} voice slots, {} command bindings)",
        keymap.path().display(),
        pocketstage_server::KEYMAP_SCHEMA,
        keymap.get().voices.len(),
        keymap.get().commands.len(),
    );
    let http_state =
        AppState::embedded(Arc::clone(&store), ctx, Arc::clone(&live), take_rx, edit_rx)
            .with_presets(presets)
            .with_keymap(Arc::clone(&keymap))
            .with_mapping_slot(Arc::clone(&mapping));
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
                // All snapshot values are PER-WINDOW counters (reset each
                // 2 s): `cycles` ~375 is healthy at 5.33 ms periods, and
                // `events=0` simply means the queue was idle this window.
                // Do not read them as cumulative — cost a debugging scare.
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
        run_live_midi(
            tx,
            Arc::clone(&stats),
            Arc::clone(&keymap),
            Arc::clone(&mapping),
        );
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
/// audio thread stays untouched. All notes (keys AND pad-transmitted ones)
/// flow through one router: mode + active voice decide the destination.
/// Transport buttons drive the master clock.
fn run_live_midi(
    tx: mpsc::Sender<TimedEvent>,
    stats: Arc<Stats>,
    keymap: Arc<KeymapSlot>,
    mapping: Arc<pocketstage_server::MappingSlot>,
) {
    // The slot's loader already fell back to factory defaults on any bad
    // file; this only catches a bug in the factory maps themselves.
    let mut maps = mapping.get();
    if let Err(e) = maps.validate() {
        eprintln!("internal default mapping invalid: {e}");
        std::process::exit(1);
    }
    let mut music = Mapper::new(maps.music.clone());
    let mut hui = Mapper::new(maps.hui.clone());
    // Pad note base: pads are reconstructed as raw notes (unified model).
    let mut pad_base = music.mapping().pad_note_base;

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
                // Live DawKeymap view (D26 phase 3): an Arc clone per
                // message picks up hot reloads instantly — the feeder is
                // the main thread, never the render path (R5). Routing is
                // stateless per message, so a remap while a note is held
                // can orphan its note-off under the new map; accepted +
                // documented risk (specs/daw-keymap.md).
                let km = keymap.get();
                // D37 mapping hot-reload: pointer compare per message (the
                // mutex is main-thread only); on change, flush any
                // half-resolved shared-button tap as real fader data — the
                // mapper's ONLY cross-message state — then rebuild. A
                // remapped pad window mid-hold can orphan one note-off,
                // same accepted + documented risk class as a keymap remap.
                let fresh = mapping.get();
                if !Arc::ptr_eq(&fresh, &maps) {
                    // Flush half-resolved taps as real fader data before
                    // the old mappers are dropped (D37).
                    for a in music.flush_pending().into_iter().chain(hui.flush_pending()) {
                        if let Action::Fader { index, value } = a {
                            send_event(&tx, m.received, Event::MixFader { index, value });
                        }
                    }
                    maps = fresh;
                    music = Mapper::new(maps.music.clone());
                    hui = Mapper::new(maps.hui.clone());
                    pad_base = maps.music.pad_note_base;
                    println!(
                        "[mapping] hot-reloaded (v{}) — mappers rebuilt",
                        mapping.version()
                    );
                }
                let kport = if m.port.ends_with(",1") {
                    KeymapPort::Hui
                } else {
                    KeymapPort::Music
                };
                // D36 knob lane: keymap `knobs[]` rows are matched against
                // the RAW CC (space + number + port), before and above the
                // built-in mappers — a claimed CC resolves to exactly one
                // Event::KnobParam, works in both unit modes with no overlay
                // gate (mapping is data), and silences the legacy MIX knob
                // path for that same CC (suppression below; Q4 precedent:
                // the map wins, the old binding goes inert).
                let mut knob_hit = false;
                if let MidiEvent::ControlChange { control, value, .. } = &m.event
                    && let Some(b) = km.knob_for(KeymapSource::cc_on(*control, kport), kport)
                {
                    knob_hit = true;
                    send_event(
                        &tx,
                        m.received,
                        Event::KnobParam {
                            target: b.target,
                            value: *value,
                        },
                    );
                }
                let mut out = Vec::new();
                if m.port.ends_with(",1") {
                    hui.map(&m.event, &mut out);
                } else {
                    music.map(&m.event, &mut out);
                }
                for action in out {
                    match action {
                        // Unified note router (user rule: pads are NOT
                        // different from notes — same port, same channel,
                        // notes 36–51). Reconstruct the raw note from Key
                        // AND Pad actions; one routing path for everything.
                        action @ (Action::Key { .. } | Action::Pad { .. }) => {
                            let (note, on, velocity) = match action {
                                Action::Key { note, on, velocity } => (note, on, velocity),
                                Action::Pad {
                                    index,
                                    on,
                                    velocity,
                                } => (pad_base + index - 1, on, velocity),
                                _ => continue,
                            };
                            if mode == Mode::Daw {
                                // Keymap-driven browser (phase 3): voice
                                // bindings select + audition at the pressed
                                // pitch, command bindings are momentary,
                                // everything else is inert. With the default
                                // map this routes exactly like the old
                                // white_slot/VOICES table.
                                match route_daw_note(&km, note, kport) {
                                    DawRoute::Voice { voice, label } => {
                                        if on {
                                            println!("[daw] select {label}");
                                            send_event(
                                                &tx,
                                                m.received,
                                                Event::SelectVoice { voice },
                                            );
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
                                    DawRoute::Command(cmd) if on => match cmd {
                                        KeymapCommand::MixToggle => {
                                            send_event(&tx, m.received, Event::ToggleMixOverlay)
                                        }
                                        KeymapCommand::SeqToggle => {
                                            send_event(&tx, m.received, Event::ToggleSeqOverlay)
                                        }
                                        // Harmless unless SEQ is live (the
                                        // core gates; SEQ can't be in DAW).
                                        KeymapCommand::SeqNextBar => {
                                            send_event(&tx, m.received, Event::SeqNextBar)
                                        }
                                        KeymapCommand::SeqFlip => {
                                            send_event(&tx, m.received, Event::SeqFlipWindow)
                                        }
                                        // No-op BY RULING (D30): ‹/› are
                                        // DAW-mode-only on this unit, so MIX
                                        // can never be paged from them, and 8
                                        // channels fit one page. The commands
                                        // stay in the frozen vocabulary for
                                        // wire-format stability only.
                                        KeymapCommand::BankPrev | KeymapCommand::BankNext => {}
                                    },
                                    _ => {}
                                }
                            } else {
                                // PRESET: overlay commands (while live),
                                // then the SEQ programmer's low octave, then
                                // the ONE voice.
                                // Q4: overlay commands follow the keymap
                                // remap; instrument routing below stays
                                // hardcoded BY DESIGN (D26 invariant).
                                let ov = stats.live.overlay.load(Ordering::Relaxed);
                                let cmd = km.command_for(KeymapSource::note(note), kport);
                                if ov != 0 && on && cmd == Some(KeymapCommand::MixToggle) {
                                    send_event(&tx, m.received, Event::ToggleMixOverlay);
                                } else if ov != 0 && on && cmd == Some(KeymapCommand::SeqToggle) {
                                    send_event(&tx, m.received, Event::ToggleSeqOverlay);
                                } else if ov == 2 && on && cmd == Some(KeymapCommand::SeqNextBar) {
                                    send_event(&tx, m.received, Event::SeqNextBar);
                                } else if ov == 2 && on && cmd == Some(KeymapCommand::SeqFlip) {
                                    send_event(&tx, m.received, Event::SeqFlipWindow);
                                } else if ov == 2 && (36..=43).contains(&note) {
                                    // SEQ programmer (D23): the low octave
                                    // — what the pads always sent — toggles
                                    // the 8-step window.
                                    if on {
                                        send_event(
                                            &tx,
                                            m.received,
                                            Event::SeqStep { step: note - 35 },
                                        );
                                    }
                                } else if stats.live.active_voice.load(Ordering::Relaxed)
                                    == VOICE_DRUMS
                                {
                                    // DRUMS voice: notes 36–51 ARE the kit
                                    // (Q3) — identical whether from a pad or
                                    // the low keys; other notes are silent.
                                    if (36..=51).contains(&note) {
                                        let slot = note - 36;
                                        let packed = if slot < 8 {
                                            stats.drum_kit.load(Ordering::Relaxed)
                                        } else {
                                            stats.drum_kit_2.load(Ordering::Relaxed)
                                        };
                                        let kit = core::kit_or_default(packed);
                                        let key = kit[(slot % 8) as usize] as i32;
                                        // Channels mirror per pair (pad 9 ≙
                                        // pad 1 kill group — documented).
                                        let chan = core::PAD_CH_BASE + (slot % 8) as i32;
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
                                } else {
                                    // Melodic voice: every note plays it —
                                    // all 49 keys and the pad notes alike
                                    // (Q2), no special cases.
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
                                            "DAW — manage (white keys=voices, G#3/A#3=overlays)"
                                        }
                                        Mode::Preset => {
                                            "PRESET — perform (one voice at a time)"
                                        }
                                    }
                                );
                                if new_mode == Mode::Daw {
                                    // D21/D25: the manage mode is a clean
                                    // slate — exit MIX/SEQ. Overlay ENTRY is
                                    // CMD_MIX/CMD_SEQ (G#3/A#3) in both
                                    // modes; an overlay armed in DAW survives
                                    // the trip back to PRESET.
                                    send_event(&tx, m.received, Event::ExitOverlays);
                                    // D15: recording stops here; playback never does.
                                    send_event(&tx, m.received, Event::StopRecording);
                                }
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
                            // Suppressed when a keymap knob row claimed this
                            // CC — the ParamMove already went out (D36).
                            if !knob_hit {
                                send_event(&tx, m.received, Event::MixKnob { index, value })
                            }
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
                        // Damper pedal (measured CC64 on the keys channel,
                        // session 16e). Music port only: the HUI port carries
                        // Mackie traffic whose CCs are not feet.
                        Action::Sustain { down } if matches!(kport, KeymapPort::Music) => {
                            send_event(&tx, m.received, Event::Sustain { down })
                        }
                        // Mod wheel, pitch lever, pad note-offs, HUI-port CCs:
                        // no engine effect yet.
                        _ => {}
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

#[cfg(test)]
mod keymap_routing_tests {
    use super::*;

    fn default_km() -> DawKeymap {
        DawKeymap::default()
    }

    #[test]
    fn defaults_route_like_the_old_hardcoded_table() {
        // Regression lock for the deleted white_slot/VOICES/CMD_* constants:
        // 12 white-key slots C2..G3 (Drums first, curated GM after), the
        // four command notes, everything else inert in DAW mode.
        let km = default_km();
        let expect: [(u8, ActiveVoice); 12] = [
            (36, ActiveVoice::Drums),
            (38, ActiveVoice::Program(0)),
            (40, ActiveVoice::Program(4)),
            (41, ActiveVoice::Program(6)),
            (43, ActiveVoice::Program(11)),
            (45, ActiveVoice::Program(19)),
            (47, ActiveVoice::Program(24)),
            (48, ActiveVoice::Program(29)),
            (50, ActiveVoice::Program(32)),
            (52, ActiveVoice::Program(38)),
            (53, ActiveVoice::Program(48)),
            (55, ActiveVoice::Program(65)),
        ];
        for (note, voice) in expect {
            match route_daw_note(&km, note, KeymapPort::Music) {
                DawRoute::Voice { voice: v, .. } => assert_eq!(v, voice, "note {note}"),
                other => panic!("note {note}: expected voice, got {other:?}"),
            }
        }
        let cmds = [
            (56, KeymapCommand::MixToggle),
            (58, KeymapCommand::SeqToggle),
            (61, KeymapCommand::SeqNextBar),
            (63, KeymapCommand::SeqFlip),
        ];
        for (note, cmd) in cmds {
            assert_eq!(
                route_daw_note(&km, note, KeymapPort::Music),
                DawRoute::Command(cmd),
                "command note {note}"
            );
        }
        // Old inert set: black keys below the span edge + everything above.
        for inert in [37u8, 39, 42, 44, 46, 49, 57, 60, 84, 127] {
            assert_eq!(
                route_daw_note(&km, inert, KeymapPort::Music),
                DawRoute::Inert,
                "inert note {inert}"
            );
        }
    }

    #[test]
    fn custom_map_reroutes_notes() {
        // Black keys become legal browser slots; commands can move. This is
        // exactly what the hardcoded table could not express (D26 goal).
        let json = r#"{
            "voices": [ { "source": {"note": 72}, "voice": {"program": 89}, "label": "Pad" } ],
            "commands": [ { "source": {"note": 36}, "command": "mix_toggle" } ],
            "fader_buttons": [],
            "knobs": []
        }"#;
        let km = DawKeymap::from_json_str(json).unwrap();
        match route_daw_note(&km, 72, KeymapPort::Music) {
            DawRoute::Voice { voice, label } => {
                assert_eq!(voice, ActiveVoice::Program(89));
                assert_eq!(label, "Pad");
            }
            other => panic!("expected voice, got {other:?}"),
        }
        assert_eq!(
            route_daw_note(&km, 36, KeymapPort::Music),
            DawRoute::Command(KeymapCommand::MixToggle)
        );
        // The old defaults are GONE once the file overrides them.
        assert_eq!(route_daw_note(&km, 40, KeymapPort::Music), DawRoute::Inert);
    }

    #[test]
    fn port_qualified_bindings_ignore_other_ports() {
        let json = r#"{
            "voices": [],
            "commands": [ { "source": {"cc": 49, "port": "hui"}, "command": "bank_prev" } ],
            "fader_buttons": [],
            "knobs": []
        }"#;
        let km = DawKeymap::from_json_str(json).unwrap();
        // Note routing never sees a CC binding…
        assert_eq!(route_daw_note(&km, 49, KeymapPort::Music), DawRoute::Inert);
        // …and the CC only answers on its declared port.
        assert_eq!(
            km.command_for(KeymapSource::cc(49), KeymapPort::Hui),
            Some(KeymapCommand::BankPrev)
        );
        assert_eq!(
            km.command_for(KeymapSource::cc(49), KeymapPort::Music),
            None
        );
    }
}
