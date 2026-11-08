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
use pocketstage_model::{
    Clip, Pattern, Project, ProjectFile, Quantize, RecordedEvent, SCHEMA_VERSION, STEPS_PER_BAR,
    StepNote, Take, Track,
};
use pocketstage_oxygen::{Action, Mapper, Mode, default_mapping, hui_mapping};
use pocketstage_server::{AppState, EngineCommand, LiveStatus, ProjectStore};

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
/// Metronome tick channel. Shares with pad 1 (kick): striking the kick
/// cuts the current click — the same collision the old single-drum-channel
/// layout had, and harmless for a 30 ms click.
const TICK_CH: i32 = 8;
/// Each pad owns one channel (pad 1..8 → 8..=15). All Sound Off is
/// per-channel, so per-pad channels make same-pad retrigger kills surgical
/// (D17). One-shot GM samples ignore note-off AND live attenuation edits
/// (lab-proven, see examples/retrigger_lab.rs); CC120 is the only reliable kill.
const PAD_CH_BASE: i32 = 8;
/// Channels reserved for pads/ticks (as opposed to lead/scene tracks on 0..=7).
#[inline]
fn is_pad_channel(chan: i32) -> bool {
    (TICK_CH..=15).contains(&chan)
}

/// Apply a track's voice state — program + mixer — to its internal channel
/// (D3: track N → channel N). Mute is CC7=0; gain 0..=2 maps onto CC7;
/// pan −1..=+1 maps onto CC10. Called at boot and on every project swap, so
/// mixer edits ride the existing PUT → SetProject path with no new plumbing.
fn apply_track_voice(synth: &ffi::Synth, chan: i32, track: &Track) {
    synth.program_change(chan, track.instrument.gm_program as i32);
    let gain = if track.mute {
        0.0
    } else {
        track.gain.unwrap_or(1.0)
    };
    synth.cc(chan, 7, (gain * 127.0).round() as i32);
    let pan = ((track.pan.unwrap_or(0.0) + 1.0) * 63.5)
        .round()
        .clamp(0.0, 127.0) as i32;
    synth.cc(chan, 10, pan);
}

/// Apply a store-validated project swap to the audio thread's state.
/// Takes adopt the incoming order/mute flags (a PUT can also delete); takes
/// finalized this boot that the store has not absorbed yet are kept — the
/// take-feed drains on its own tick and must not lose one to a racing swap.
fn apply_swap(
    pf: ProjectFile,
    project: &mut Project,
    bpm: &mut f64,
    takes: &mut Vec<Take>,
    mine: &[String],
    synth: &ffi::Synth,
) {
    let mut new = pf.project;
    let mut adopted = std::mem::take(&mut new.takes);
    for t in takes.drain(..) {
        if mine.contains(&t.name) && !adopted.iter().any(|a| a.name == t.name) {
            adopted.push(t);
        }
    }
    *takes = adopted;
    *bpm = new.bpm;
    for (t, track) in new.tracks.iter().enumerate() {
        apply_track_voice(synth, t as i32, track);
    }
    *project = new;
}

/// Fixed 8-hit GM kit for PRESET-mode pads (D12): kick snare rim clap chh ohh tom crash.
/// JSON-reassignable kits are on the roadmap; constants until then.
const DRUM_KIT: [u8; 8] = [36, 38, 37, 39, 42, 46, 47, 49];

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
    /// ⏺ — start capturing a take (starts the clock if stopped); metronome
    /// runs only while recording (D16).
    StartRecording,
    /// D15: entering DAW mode finalizes any live take but leaves playback,
    /// scene, and sounding notes completely alone.
    StopRecording,
    /// D13: DAW-mode selector — the perform voice (lead channel) takes a new
    /// GM program; the pressed key itself is forwarded as an audition.
    SetLeadProgram {
        program: u8,
    },
    /// D13 settings key (letter B in DAW mode): cycle loop_bars 2→4→8→16→2.
    CycleLoopBars,
}

/// Wrap an engine event that arrived from the HTTP lane as a TimedEvent
/// (both timestamps are "now" — latency for a web click is irrelevant).
fn remote(event: Event) -> TimedEvent {
    TimedEvent {
        intended: Instant::now(),
        sent: Instant::now(),
        event,
    }
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
    // Transport *levels* — shared with the HTTP/WS layer (Arc'd so the
    // server reads the very same atomics the audio thread writes).
    live: Arc<LiveStatus>,
    /// Live project swaps via HTTP (cumulative).
    project_updates: std::sync::atomic::AtomicU64,
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
    // Pads + metronome live on channels 8..=15; make them percussion. Note:
    // set_channel_type alone does NOT re-resolve the channel's preset — the
    // bank_select + program_change pair is what actually lands them on the
    // GM drum kit (lab: examples/drum_chan_lab.rs).
    for ch in TICK_CH..=15 {
        synth.set_channel_type_drum(ch);
        synth.bank_select(ch, 128); // GM percussion bank
        synth.program_change(ch, 0);
    }
    // ---- boot project: load from the store (single source of truth) ------
    // Same file the HTTP control plane reads/writes; PUTs hot-swap the live
    // project via EngineCommand. Precedence: POCKETSTAGE_PROJECT > ~/.pocketstage.
    let project_path = std::env::var("POCKETSTAGE_PROJECT").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.pocketstage/project.json")
    });
    let store = Arc::new(
        ProjectStore::open_or_default(&project_path).unwrap_or_else(|e| {
            eprintln!("[init] cannot open project '{project_path}': {e}");
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
    let http_state = AppState::embedded(Arc::clone(&store), ctx, Arc::clone(&live), take_rx);
    match pocketstage_server::spawn_engine_server(http_state, ([0, 0, 0, 0], port).into()) {
        Ok(()) => println!("[server] control plane on http://0.0.0.0:{port} (LAN-trusted)"),
        Err(e) => eprintln!("[server] NOT started ({e}) — audio unaffected"),
    }

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

            // Track programs + mixer (D3: track N → internal channel N).
            // Keys on channel 0 thus play track 1's instrument (and level).
            for (t, track) in project.tracks.iter().enumerate() {
                apply_track_voice(&synth, t as i32, track);
            }

            let mut bpm = project.bpm;
            let mut cycle: u64 = 0;
            let mut total_samples: u64 = 0; // frames rendered so far (master clock domain)
            let mut playing_since: Option<u64> = None; // sample index where Play fired
            let mut active_scene: Option<usize> = None;
            let mut pending_launch: Option<(usize, Quantize)> = None; // (scene, quantize)
            // Recording (D16): (base step, events with take-local ticks).
            // Capture happens in the drain loop — the audio thread owns both
            // the clock and the event stream, so stamping is exact and needs
            // no atomics or main-thread changes.
            let mut recording: Option<(u64, Vec<RecordedEvent>)> = None;
            // Takes persisted in the project (earlier sessions) are live from
            // t=0; new finalizes append to the file via the take feed.
            let mut takes: Vec<Take> = std::mem::take(&mut project.takes);
            // Names of takes finalized *this boot*: they survive a project swap
            // until the take-feed has persisted them (swap-vs-drain race), but
            // boot-seeded takes are purely store-owned and vanish when a PUT
            // drops or mutes them.
            let mut mine: Vec<String> = Vec::new();
            // Remote (web) transport/scene commands, translated to Events in
            // step 0 and consumed by the same drain as hardware events.
            let mut remote_ev: std::collections::VecDeque<TimedEvent> = Default::default();
            a_stats
                .live
                .takes
                .store(takes.len().min(u8::MAX as usize) as u8, Ordering::Relaxed);
            loop {
                // 0) HTTP commands. Payloads are store-validated; the swap +
                // dealloc runs here (user-initiated, rare) — deliberate
                // exception to the audio-thread no-allocation rule. Transport/
                // scene triggers are translated into ordinary events and run
                // through the same pipeline as hardware (step 1).
                remote_ev.clear();
                for _ in 0..16 {
                    let Ok(cmd) = crx.try_recv() else { break };
                    match cmd {
                        EngineCommand::SetProject(pf) => {
                            apply_swap(pf, &mut project, &mut bpm, &mut takes, &mine, &synth);
                            a_stats
                                .project_updates
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        EngineCommand::Play => remote_ev.push_back(remote(Event::Play)),
                        EngineCommand::Stop => remote_ev.push_back(remote(Event::Stop)),
                        EngineCommand::Record => {
                            remote_ev.push_back(remote(Event::StartRecording));
                        }
                        EngineCommand::LaunchScene(n) if n >= 1 => {
                            remote_ev.push_back(remote(Event::LaunchScene {
                                index: n as usize - 1,
                            }));
                        }
                        _ => {} // non_exhaustive: future commands ignored
                    }
                }

                // 1) Drain pending events: queued remote commands first, then
                // the MIDI lane (bounded per cycle — writes stay timely).
                for _ in 0..80 {
                    let te = match remote_ev.pop_front().or_else(|| rx.try_recv().ok()) {
                        Some(te) => te,
                        None => break,
                    };
                    {
                        {
                            let q_us = te.sent.elapsed().as_micros() as u64;
                            let feed_us = te.intended.elapsed().as_micros() as u64 - q_us;
                            a_stats.events.fetch_add(1, Ordering::Relaxed);
                            a_stats.q_sum_us.fetch_add(q_us, Ordering::Relaxed);
                            a_stats.q_max_us.fetch_max(q_us, Ordering::Relaxed);
                            a_stats.feed_max_us.fetch_max(feed_us, Ordering::Relaxed);
                            match te.event {
                                Event::NoteOn { chan, key, vel } => {
                                    // Per-key retrigger (D17): a strike resets
                                    // the previous voice of this exact key;
                                    // other keys stay fully polyphonic.
                                    // Pads: All Sound Off on their private
                                    // channel (one-shots ignore note-off).
                                    // Leads: note-off works (release stage).
                                    if is_pad_channel(chan) {
                                        synth.all_sounds_off(chan);
                                    } else {
                                        synth.note_off(chan, key);
                                    }
                                    synth.note_on(chan, key, vel);
                                    // Take capture: everything sounding while
                                    // recording is stamped on the master clock.
                                    if let Some((base, evs)) = recording.as_mut()
                                        && let Some(start) = playing_since
                                    {
                                        let step =
                                            clock::steps_elapsed(total_samples - start, RATE, bpm)
                                                as u64;
                                        evs.push(RecordedEvent::NoteOn {
                                            tick: step.saturating_sub(*base) as u32,
                                            channel: chan as u8,
                                            pitch: key as u8,
                                            velocity: vel as u8,
                                        });
                                    }
                                }
                                Event::NoteOff { chan, key } => {
                                    synth.note_off(chan, key);
                                    if let Some((base, evs)) = recording.as_mut()
                                        && let Some(start) = playing_since
                                    {
                                        let step =
                                            clock::steps_elapsed(total_samples - start, RATE, bpm)
                                                as u64;
                                        evs.push(RecordedEvent::NoteOff {
                                            tick: step.saturating_sub(*base) as u32,
                                            channel: chan as u8,
                                            pitch: key as u8,
                                        });
                                    }
                                }
                                Event::Play => {
                                    if playing_since.is_none() {
                                        playing_since = Some(total_samples);
                                    }
                                }
                                Event::Stop => {
                                    // Finalize any live take first (D15: stop
                                    // means silent; playback waits for Play).
                                    if let Some((_base, evs)) = recording.take()
                                        && !evs.is_empty()
                                    {
                                        let take = Take {
                                            name: format!("Take {}", takes.len() + 1),
                                            events: evs,
                                            muted: false,
                                        };
                                        a_stats.live.takes.fetch_add(1, Ordering::Relaxed);
                                        // Hand the finalized take to the HTTP layer
                                        // (clone + try_send: user-initiated, rare).
                                        let _ = take_feed.send(take.clone());
                                        mine.push(take.name.clone());
                                        takes.push(take);
                                    }
                                    a_stats.live.recording.store(false, Ordering::Relaxed);
                                    playing_since = None;
                                    active_scene = None;
                                    pending_launch = None;
                                    a_stats.live.active_scene.store(0, Ordering::Relaxed);
                                    for t in 0..project.tracks.len() as i32 {
                                        synth.all_notes_off(t);
                                    }
                                }
                                Event::StartRecording => {
                                    if recording.is_none() {
                                        if playing_since.is_none() {
                                            playing_since = Some(total_samples);
                                        }
                                        let base = clock::steps_elapsed(
                                            total_samples - playing_since.unwrap_or(total_samples),
                                            RATE,
                                            bpm,
                                        ) as u64;
                                        recording = Some((base, Vec::new()));
                                        a_stats.live.recording.store(true, Ordering::Relaxed);
                                    }
                                }
                                Event::StopRecording => {
                                    // D15: finalize the take if one is live;
                                    // playback, scene and held notes untouched.
                                    if let Some((_base, evs)) = recording.take()
                                        && !evs.is_empty()
                                    {
                                        let take = Take {
                                            name: format!("Take {}", takes.len() + 1),
                                            events: evs,
                                            muted: false,
                                        };
                                        a_stats.live.takes.fetch_add(1, Ordering::Relaxed);
                                        let _ = take_feed.send(take.clone());
                                        mine.push(take.name.clone());
                                        takes.push(take);
                                    }
                                    a_stats.live.recording.store(false, Ordering::Relaxed);
                                }
                                Event::SetLeadProgram { program } => {
                                    synth.program_change(LEAD_CH, program as i32);
                                }
                                Event::CycleLoopBars => {
                                    project.loop_bars = match project.loop_bars {
                                        2 => 4,
                                        4 => 8,
                                        8 => 16,
                                        _ => 2,
                                    };
                                }
                                Event::LaunchScene { index } => {
                                    let q =
                                        project.scenes[index].quantize.unwrap_or(Quantize::NextBar);
                                    if playing_since.is_none() {
                                        // Launching from stopped starts playback now.
                                        playing_since = Some(total_samples);
                                        active_scene = Some(index);
                                        a_stats
                                            .live
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
                                            .live
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
                                a_stats
                                    .live
                                    .active_scene
                                    .store(i as u8 + 1, Ordering::Relaxed);
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
                            // Metronome (D16): fires ONLY while recording —
                            // never during plain playback, never captured.
                            if recording.is_some() {
                                // Accent the downbeat; kick self-decays.
                                let vel: i32 = if p.is_multiple_of(4) { 110 } else { 70 };
                                synth.note_on(TICK_CH, 36, vel);
                                synth.note_off(TICK_CH, 36);
                            }
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

                        // Take playback (D16): finalized takes loop once
                        // recording is off (no overdub in v0.1).
                        if recording.is_none() {
                            let ts0 = clock::steps_elapsed(total_samples - start, RATE, bpm);
                            let ts1 = clock::steps_elapsed(
                                total_samples + PERIOD_FRAMES as u64 - start,
                                RATE,
                                bpm,
                            );
                            let mut k = ts0.ceil() as u64;
                            while (k as f64) < ts1 {
                                for take in &takes {
                                    if take.muted {
                                        continue; // kept but silent (take mgmt)
                                    }
                                    let lt = take.loop_ticks(project.loop_bars) as u64;
                                    for ev in take.events_at((k % lt) as u32) {
                                        match *ev {
                                            RecordedEvent::NoteOn {
                                                channel,
                                                pitch,
                                                velocity,
                                                ..
                                            } => {
                                                // Same retrigger semantics on
                                                // take replay as live input:
                                                // cuts last loop's still-
                                                // ringing strikes.
                                                if is_pad_channel(channel as i32) {
                                                    synth.all_sounds_off(channel as i32);
                                                } else {
                                                    synth.note_off(channel as i32, pitch as i32);
                                                }
                                                synth.note_on(
                                                    channel as i32,
                                                    pitch as i32,
                                                    velocity as i32,
                                                )
                                            }
                                            RecordedEvent::NoteOff { channel, pitch, .. } => {
                                                synth.note_off(channel as i32, pitch as i32)
                                            }
                                            RecordedEvent::ProgramChange {
                                                channel,
                                                program,
                                                ..
                                            } => {
                                                synth.program_change(channel as i32, program as i32)
                                            }
                                        }
                                    }
                                }
                                k += 1;
                            }
                        }

                        a_stats.live.playing.store(true, Ordering::Relaxed);
                        a_stats
                            .live
                            .pos_beats_x1000
                            .store((b1 * 1000.0) as u32, Ordering::Relaxed);
                    }
                    None => {
                        a_stats.live.playing.store(false, Ordering::Relaxed);
                        a_stats.live.pos_beats_x1000.store(0, Ordering::Relaxed);
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
                                let key = DRUM_KIT[(index - 1) as usize] as i32;
                                // One channel per pad so retriggers kill only this pad.
                                let chan = PAD_CH_BASE + (index - 1) as i32;
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
