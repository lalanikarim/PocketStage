//! Engine core — the synth-agnostic half of the audio thread's work.
//!
//! This is the integration harness (backlog 13): everything the render loop
//! *decides* (event drain, transport, recording, take capture/playback,
//! project swaps) lives here behind a [`SynthBackend`] trait, so CI can drive
//! the entire event pipeline device-free with a fake backend — no ALSA, no
//! FluidSynth, no hardware. `main.rs` keeps only the parts that genuinely
//! need the platform: opening ALSA, writing bytes, the MIDI pump feeder.
//!
//! The render-loop law (D20) applies to everything in this module: no panic,
//! no trust in cross-lane indices, totals via [`Option`]. `period()` is what
//! the audio thread calls once per cycle; tests call it in a loop and assert
//! on the recorded backend calls.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use pocketstage_model::{Project, ProjectFile, Quantize, RecordedEvent, Take, Track};
use pocketstage_server::{EngineCommand, LiveStatus};

use crate::clock;
use crate::transport::Transport;

// ---------------------------------------------------------------- constants

pub const RATE: u32 = 48_000;
pub const PERIOD_FRAMES: usize = 256; // frames per period (buffer size knob)

/// Keys play track 1's voice (D3: track N → internal channel N).
pub const LEAD_CH: i32 = 0;
/// Metronome tick channel. Shares with pad 1 (kick): striking the kick
/// cuts the current click — the same collision the old single-drum-channel
/// layout had, and harmless for a 30 ms click.
pub const TICK_CH: i32 = 8;
/// Each pad owns one channel (pad 1..8 → 8..=15). All Sound Off is
/// per-channel, so per-pad channels make same-pad retrigger kills surgical
/// (D17). One-shot GM samples ignore note-off AND live attenuation edits
/// (lab-proven, see examples/retrigger_lab.rs); CC120 is the only reliable kill.
pub const PAD_CH_BASE: i32 = 8;

/// Channels reserved for pads/ticks (as opposed to lead/scene tracks on 0..=7).
#[inline]
pub fn is_pad_channel(chan: i32) -> bool {
    (TICK_CH..=15).contains(&chan)
}

/// Fixed 8-hit GM kit for PRESET-mode pads (D12): kick snare rim clap chh
/// ohh tom crash. Used when the project leaves `drum_kit` unset.
pub const DEFAULT_DRUM_KIT: [u8; 8] = [36, 38, 37, 39, 42, 46, 47, 49];

/// The live kit travels from the audio thread (boot + swaps write it) to
/// the MIDI feeder (reads it on every pad hit) as one packed AtomicU64 —
/// lock-free both ways, keeping the audio-thread no-locks rule. Layout:
/// 8 GM notes in bytes 0..=7 (validated ≤ 81, so bit 63 is always free)
/// plus [`KIT_SET`] marking "the core has written a kit at least once".
pub const KIT_SET: u64 = 1 << 63;

pub fn pack_kit(kit: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*kit) | KIT_SET
}

pub fn unpack_kit(v: u64) -> [u8; 8] {
    (v & !KIT_SET).to_le_bytes()
}

/// Reader-side resolution: the packed kit once the core has written one,
/// else the fixed default (covers feeder strikes before the core boots).
pub fn kit_or_default(v: u64) -> [u8; 8] {
    if v & KIT_SET == 0 {
        DEFAULT_DRUM_KIT
    } else {
        unpack_kit(v)
    }
}

// -------------------------------------------------------------------- synth

/// Everything the core can ask the audio engine to do. The real backend is a
/// thin wrapper over the FluidSynth FFI; tests use a call-recording fake.
pub trait SynthBackend {
    fn note_on(&mut self, chan: i32, key: i32, vel: i32);
    fn note_off(&mut self, chan: i32, key: i32);
    fn program_change(&mut self, chan: i32, program: i32);
    fn cc(&mut self, chan: i32, ctrl: i32, val: i32);
    /// Silence voices with the release stage honored (scene switches, keys).
    fn all_notes_off(&mut self, chan: i32);
    /// Kill voices instantly, one-shots included (D17 retrigger).
    fn all_sounds_off(&mut self, chan: i32);
    /// Render `frames` audio frames into separate L / R float buffers.
    fn render(&mut self, frames: usize, left: &mut [f32], right: &mut [f32]);
    /// Global reverb + chorus amounts (both normalized 0..=1) — the FX
    /// amount knobs; applied at boot and on every project swap.
    fn set_global_fx(&mut self, reverb: f64, chorus: f64);
}

/// Engine defaults when the project leaves the global FX amounts unset
/// (reverb = FluidSynth's stock 0.9; chorus 0.5 ≈ the stock level of 2).
pub const FX_REVERB_DEFAULT: f64 = 0.9;
pub const FX_CHORUS_DEFAULT: f64 = 0.5;

/// Real backend: borrowed handle to the one FluidSynth instance (owned and
/// moved into the audio thread by `main.rs` — single writer, R5).
pub struct FluidBackend<'a>(pub &'a crate::ffi::Synth);

impl SynthBackend for FluidBackend<'_> {
    #[inline]
    fn note_on(&mut self, chan: i32, key: i32, vel: i32) {
        self.0.note_on(chan, key, vel)
    }
    #[inline]
    fn note_off(&mut self, chan: i32, key: i32) {
        self.0.note_off(chan, key)
    }
    #[inline]
    fn program_change(&mut self, chan: i32, program: i32) {
        self.0.program_change(chan, program)
    }
    #[inline]
    fn cc(&mut self, chan: i32, ctrl: i32, val: i32) {
        self.0.cc(chan, ctrl, val)
    }
    #[inline]
    fn all_notes_off(&mut self, chan: i32) {
        self.0.all_notes_off(chan)
    }
    #[inline]
    fn all_sounds_off(&mut self, chan: i32) {
        self.0.all_sounds_off(chan)
    }
    #[inline]
    fn render(&mut self, frames: usize, left: &mut [f32], right: &mut [f32]) {
        self.0.render(frames, left, right)
    }
    #[inline]
    fn set_global_fx(&mut self, reverb: f64, chorus: f64) {
        self.0.set_global_fx(reverb, chorus)
    }
}

// ------------------------------------------------------------------- events

#[derive(Debug, Clone, Copy)]
pub enum Event {
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

/// An event plus the wall-clock time it was *intended* to fire. The audio
/// thread records (consume_time - intended) as scheduling latency.
pub struct TimedEvent {
    pub intended: Instant, // when the feeder meant to emit it (feeder jitter = send - intended)
    pub sent: Instant,     // when it hit the channel (engine latency = consume - sent)
    pub event: Event,
}

impl TimedEvent {
    /// Both timestamps are "now" — remote (HTTP) events and tests don't care
    /// about latency attribution.
    pub fn now(event: Event) -> Self {
        let t = Instant::now();
        TimedEvent {
            intended: t,
            sent: t,
            event,
        }
    }
}

// -------------------------------------------------------------------- stats

#[derive(Default)]
pub struct Stats {
    pub cycles: AtomicU64,           // render+write iterations
    pub busy_ns: AtomicU64, // time spent in synth render + interleave (not blocking writes)
    pub xruns: AtomicU64,   // ALSA underruns recovered
    pub events: AtomicU64,  // MIDI events consumed
    pub q_sum_us: AtomicU64, // engine-internal queue wait: consumed - sent
    pub q_max_us: AtomicU64, // worst queue wait in window
    pub feed_max_us: AtomicU64, // worst feeder jitter (sent - intended) in window
    pub delay_frames_max: AtomicU64, // max snd_pcm_delay seen in window (output-side queue)
    /// Transport *levels* — shared with the HTTP/WS layer (Arc'd so the
    /// server reads the very same atomics the audio thread writes).
    pub live: Arc<LiveStatus>,
    /// Live project swaps via HTTP (cumulative).
    pub project_updates: AtomicU64,
    /// Packed PRESET drum kit for the pad feeder ([`pack_kit`] bits, plus
    /// [`KIT_SET`] once written). Written by the audio thread at boot and
    /// on every swap; read lock-free by the MIDI feeder on pad hits.
    pub drum_kit: AtomicU64,
}

impl Stats {
    pub fn snapshot_and_reset(&self) -> [u64; 8] {
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

// ------------------------------------------------------------ voice helpers

/// Apply a track's voice state — program + mixer — to its internal channel
/// (D3: track N → channel N). Mute is CC7=0; gain 0..=2 maps onto CC7;
/// pan −1..=+1 maps onto CC10. Called at boot and on every project swap, so
/// mixer edits ride the existing PUT → SetProject path with no new plumbing.
pub fn apply_track_voice<S: SynthBackend>(synth: &mut S, chan: i32, track: &Track) {
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
    // FX sends: None = leave the soundfont's own preset value untouched.
    if let Some(v) = track.reverb {
        synth.cc(chan, 91, (v * 127.0).round() as i32);
    }
    if let Some(v) = track.chorus {
        synth.cc(chan, 93, (v * 127.0).round() as i32);
    }
}

/// Voice reset on scene switch: silence every track; (re-)set the program
/// only for tracks that actually have a clip in this column.
pub fn apply_scene_voice<S: SynthBackend>(synth: &mut S, project: &Project, scene: usize) {
    for (t, track) in project.tracks.iter().enumerate() {
        synth.all_notes_off(t as i32);
        if track.clips.get(scene).is_some_and(|c| c.is_some()) {
            synth.program_change(t as i32, track.instrument.gm_program as i32);
        }
    }
}

// ----------------------------------------------------------------- the core

/// Per-period state machine of the audio thread. Constructed once and driven
/// exclusively from one thread (the real audio thread or a test loop) —
/// single writer, no locks anywhere in here.
pub struct EngineCore {
    pub project: Project,
    pub bpm: f64,
    pub takes: Vec<Take>,
    /// Names of takes finalized *this boot*: they survive a project swap
    /// until the take-feed has persisted them (swap-vs-drain race), but
    /// boot-seeded takes are purely store-owned and vanish when a PUT
    /// drops or mutes them.
    pub mine: Vec<(String, Instant)>,
    /// Recording (D16): (base step, events with take-local ticks).
    pub recording: Option<(u64, Vec<RecordedEvent>)>,
    /// Scene/launch state machine — pure & unit-tested (transport.rs).
    pub transport: Transport,
    pub total_samples: u64, // frames rendered so far (master clock domain)
    pub playing_since: Option<u64>, // sample index where Play fired
    pub cycles: u64,
    pub live: Arc<LiveStatus>,
    pub stats: Arc<Stats>,
    /// Finalized takes flow back to the HTTP layer (recordings survive
    /// reboot); tests keep the receiving end.
    take_feed: Sender<Take>,
    left: Vec<f32>,
    right: Vec<f32>,
    out: Vec<i16>, // interleaved S16LE, ready for ALSA
}

impl EngineCore {
    pub fn new(
        mut project: Project,
        live: Arc<LiveStatus>,
        stats: Arc<Stats>,
        take_feed: Sender<Take>,
    ) -> Self {
        // Takes persisted in the project (earlier sessions) are live from t=0.
        let takes = std::mem::take(&mut project.takes);
        live.takes
            .store(takes.len().min(u8::MAX as usize) as u8, Ordering::Relaxed);
        stats.drum_kit.store(
            pack_kit(&project.drum_kit.unwrap_or(DEFAULT_DRUM_KIT)),
            Ordering::Relaxed,
        );
        Self {
            bpm: project.bpm,
            project,
            takes,
            mine: Vec::new(),
            recording: None,
            transport: Transport::default(),
            total_samples: 0,
            playing_since: None,
            cycles: 0,
            live,
            stats,
            take_feed,
            left: vec![0.0; PERIOD_FRAMES],
            right: vec![0.0; PERIOD_FRAMES],
            out: vec![0; PERIOD_FRAMES * 2],
        }
    }

    /// Boot-time: apply every track's program + mixer to its channel,
    /// plus the global FX amounts.
    pub fn apply_all_voices<S: SynthBackend>(&self, synth: &mut S) {
        for (t, track) in self.project.tracks.iter().enumerate() {
            apply_track_voice(synth, t as i32, track);
        }
        self.apply_global_fx(synth);
    }

    /// Global FX amount knobs: project values (or engine defaults) onto the
    /// synth. Rides the boot + SetProject paths — no new command needed.
    fn apply_global_fx<S: SynthBackend>(&self, synth: &mut S) {
        synth.set_global_fx(
            self.project
                .fx_reverb
                .unwrap_or(FX_REVERB_DEFAULT)
                .clamp(0.0, 1.0),
            self.project
                .fx_chorus
                .unwrap_or(FX_CHORUS_DEFAULT)
                .clamp(0.0, 1.0),
        );
    }

    /// Interleaved S16LE output of the most recent [`Self::period`] — hand
    /// this to ALSA.
    pub fn output(&self) -> &[i16] {
        &self.out
    }

    /// One audio cycle: HTTP commands → event drain → master clock (ticks,
    /// patterns, take loops) → render. The caller never reorders these;
    /// tests call it in a loop to simulate time.
    pub fn period<S: SynthBackend>(
        &mut self,
        synth: &mut S,
        cmds: &Receiver<EngineCommand>,
        events: &Receiver<TimedEvent>,
    ) {
        // 0) HTTP commands. Payloads are store-validated; the swap + dealloc
        // runs here (user-initiated, rare) — deliberate exception to the
        // audio-thread no-allocation rule. Transport/scene triggers are
        // translated into ordinary events and run through the same pipeline
        // as hardware (step 1).
        let mut remote_ev: std::collections::VecDeque<TimedEvent> =
            std::collections::VecDeque::new();
        for _ in 0..16 {
            let Ok(cmd) = cmds.try_recv() else { break };
            match cmd {
                EngineCommand::SetProject(pf) => self.apply_swap(synth, pf),
                EngineCommand::Play => remote_ev.push_back(TimedEvent::now(Event::Play)),
                EngineCommand::Stop => remote_ev.push_back(TimedEvent::now(Event::Stop)),
                EngineCommand::Record => {
                    remote_ev.push_back(TimedEvent::now(Event::StartRecording));
                }
                EngineCommand::LaunchScene(n) if n >= 1 => {
                    remote_ev.push_back(TimedEvent::now(Event::LaunchScene {
                        index: n as usize - 1,
                    }));
                }
                EngineCommand::RenameTake { from, to } => self.rename_take(&from, &to),
                _ => {} // non_exhaustive: future commands ignored
            }
        }

        // 1) Drain pending events: queued remote commands first, then the
        // MIDI lane (bounded per cycle — writes stay timely).
        for _ in 0..80 {
            let te = match remote_ev.pop_front().or_else(|| events.try_recv().ok()) {
                Some(te) => te,
                None => break,
            };
            let q_us = te.sent.elapsed().as_micros() as u64;
            let feed_us = te.intended.elapsed().as_micros() as u64 - q_us;
            self.stats.events.fetch_add(1, Ordering::Relaxed);
            self.stats.q_sum_us.fetch_add(q_us, Ordering::Relaxed);
            self.stats.q_max_us.fetch_max(q_us, Ordering::Relaxed);
            self.stats.feed_max_us.fetch_max(feed_us, Ordering::Relaxed);
            self.handle_event(synth, te.event);
        }

        // 1b) Master clock: ticks, scene-pattern gating, take loops.
        self.clock_section(synth);

        // 2) Render float L/R -> interleave + soft-clip to i16.
        let t0 = Instant::now();
        synth.render(PERIOD_FRAMES, &mut self.left, &mut self.right);
        for i in 0..PERIOD_FRAMES {
            let l = (self.left[i] * i32::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
            let r = (self.right[i] * i32::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
            self.out[2 * i] = l as i16;
            self.out[2 * i + 1] = r as i16;
        }
        let busy = t0.elapsed().as_nanos() as u64;

        self.total_samples += PERIOD_FRAMES as u64;
        self.cycles += 1;
        self.stats.cycles.fetch_add(1, Ordering::Relaxed);
        self.stats.busy_ns.fetch_add(busy, Ordering::Relaxed);
    }

    /// Apply a store-validated project swap. Takes adopt the incoming
    /// order/mute flags (a PUT can also delete); takes finalized this boot
    /// that the store has not absorbed yet are kept — the take-feed drains on
    /// its own tick and must not lose one to a racing swap.
    fn apply_swap<S: SynthBackend>(&mut self, synth: &mut S, pf: ProjectFile) {
        let mut new = pf.project;
        // "Mine" only protects a take during the short window between finalize
        // and store absorption (the feed drains every 250 ms). Beyond that a
        // PUT deletion — e.g. clear-all — must actually delete, not resurrect.
        self.mine.retain(|(_, when)| when.elapsed().as_secs() < 30);
        let mut adopted = std::mem::take(&mut new.takes);
        for t in self.takes.drain(..) {
            if self
                .mine
                .iter()
                .any(|(n, _)| *n == t.name && !adopted.iter().any(|a| a.name == t.name))
            {
                adopted.push(t);
            }
        }
        self.takes = adopted;
        self.bpm = new.bpm;
        for (t, track) in new.tracks.iter().enumerate() {
            apply_track_voice(synth, t as i32, track);
        }
        // A PUT may shrink the scene list; drop any dangling active/pending
        // launch before it becomes the project's truth.
        if self.transport.clamp_scenes(new.scenes.len()) {
            self.live.active_scene.store(0, Ordering::Relaxed);
        }
        self.project = new;
        self.stats.drum_kit.store(
            pack_kit(&self.project.drum_kit.unwrap_or(DEFAULT_DRUM_KIT)),
            Ordering::Relaxed,
        );
        self.apply_global_fx(synth);
        self.stats
            .project_updates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Rename a live take (and its "mine" entry so the swap keep-rule keeps
    /// matching after the store-side rename). Adoption keys on names, so
    /// both copies must move together or the next swap duplicates.
    fn rename_take(&mut self, from: &str, to: &str) {
        for t in &mut self.takes {
            if t.name == from {
                t.name = to.to_string();
            }
        }
        for (n, _) in &mut self.mine {
            if n == from {
                *n = to.to_string();
            }
        }
    }

    /// Finalize the live recording (if any) into a named take. Shared by
    /// Stop and D15's StopRecording.
    fn finalize_take(&mut self) {
        if let Some((_base, evs)) = self.recording.take()
            && !evs.is_empty()
        {
            let take = Take {
                name: format!("Take {}", self.takes.len() + 1),
                events: evs,
                muted: false,
            };
            self.live.takes.fetch_add(1, Ordering::Relaxed);
            // Hand the finalized take to the HTTP layer (clone + try_send:
            // user-initiated, rare).
            let _ = self.take_feed.send(take.clone());
            self.mine.push((take.name.clone(), Instant::now()));
            self.takes.push(take);
        }
        self.live.recording.store(false, Ordering::Relaxed);
    }

    fn handle_event<S: SynthBackend>(&mut self, synth: &mut S, event: Event) {
        match event {
            Event::NoteOn { chan, key, vel } => {
                // Per-key retrigger (D17): a strike resets the previous voice
                // of this exact key; other keys stay fully polyphonic.
                // Pads: All Sound Off on their private channel (one-shots
                // ignore note-off). Leads: note-off works (release stage).
                if is_pad_channel(chan) {
                    synth.all_sounds_off(chan);
                } else {
                    synth.note_off(chan, key);
                }
                synth.note_on(chan, key, vel);
                self.capture_note_on(chan, key, vel);
            }
            Event::NoteOff { chan, key } => {
                synth.note_off(chan, key);
                if let Some((base, evs)) = self.recording.as_mut()
                    && let Some(start) = self.playing_since
                {
                    let step =
                        clock::steps_elapsed(self.total_samples - start, RATE, self.bpm) as u64;
                    evs.push(RecordedEvent::NoteOff {
                        tick: step.saturating_sub(*base) as u32,
                        channel: chan as u8,
                        pitch: key as u8,
                    });
                }
            }
            Event::Play => {
                if self.playing_since.is_none() {
                    self.playing_since = Some(self.total_samples);
                    self.transport.playing = true;
                }
            }
            Event::Stop => {
                // Finalize any live take first (D15: stop means silent;
                // playback waits for Play).
                self.finalize_take();
                self.playing_since = None;
                self.transport.stop();
                self.live.active_scene.store(0, Ordering::Relaxed);
                for t in 0..self.project.tracks.len() as i32 {
                    synth.all_notes_off(t);
                }
            }
            Event::StartRecording => {
                if self.recording.is_none() {
                    if self.playing_since.is_none() {
                        self.playing_since = Some(self.total_samples);
                        self.transport.playing = true;
                    }
                    let base = clock::steps_elapsed(
                        self.total_samples - self.playing_since.unwrap_or(self.total_samples),
                        RATE,
                        self.bpm,
                    ) as u64;
                    self.recording = Some((base, Vec::new()));
                    self.live.recording.store(true, Ordering::Relaxed);
                }
            }
            Event::StopRecording => {
                // D15: finalize the take if one is live; playback, scene and
                // held notes untouched.
                self.finalize_take();
            }
            Event::SetLeadProgram { program } => {
                synth.program_change(LEAD_CH, program as i32);
            }
            Event::CycleLoopBars => {
                self.project.loop_bars = match self.project.loop_bars {
                    2 => 4,
                    4 => 8,
                    8 => 16,
                    _ => 2,
                };
            }
            Event::LaunchScene { index } => {
                // Scenes the project does not have (e.g. pad 8 after a
                // shrink) resolve to None and are IGNORED — transport.rs.
                let q = self
                    .project
                    .scenes
                    .get(index)
                    .map(|s| s.quantize.unwrap_or(Quantize::NextBar));
                match self.transport.launch(index, q) {
                    crate::transport::Effect::Start { scene } => {
                        // Launching from stopped starts playback now.
                        self.playing_since = Some(self.total_samples);
                        self.live
                            .active_scene
                            .store(scene as u8 + 1, Ordering::Relaxed);
                        apply_scene_voice(synth, &self.project, scene);
                    }
                    crate::transport::Effect::Switch { scene } => {
                        self.live
                            .active_scene
                            .store(scene as u8 + 1, Ordering::Relaxed);
                        apply_scene_voice(synth, &self.project, scene);
                    }
                    crate::transport::Effect::Queued | crate::transport::Effect::Ignore => {}
                }
            }
        }
    }

    /// Take capture: everything sounding while recording is stamped on the
    /// master clock. Split out of handle_event's NoteOn arm for readability.
    fn capture_note_on(&mut self, chan: i32, key: i32, vel: i32) {
        if let Some((base, evs)) = self.recording.as_mut()
            && let Some(start) = self.playing_since
        {
            let step = clock::steps_elapsed(self.total_samples - start, RATE, self.bpm) as u64;
            evs.push(RecordedEvent::NoteOn {
                tick: step.saturating_sub(*base) as u32,
                channel: chan as u8,
                pitch: key as u8,
                velocity: vel as u8,
            });
        }
    }

    /// Clock-driven output for one period: beat ticks (metronome while
    /// recording), 16th-gated scene patterns, looping takes (D16 + D18).
    fn clock_section<S: SynthBackend>(&mut self, synth: &mut S) {
        let Some(start) = self.playing_since else {
            self.live.playing.store(false, Ordering::Relaxed);
            self.live.pos_beats_x1000.store(0, Ordering::Relaxed);
            return;
        };

        // Beat boundaries crossed inside this period (bounded — a 256-frame
        // period spans ~1% of a beat at 120 BPM; the loop runs ≤1x).
        let b0 = clock::beats_elapsed(self.total_samples - start, RATE, self.bpm);
        let b1 = clock::beats_elapsed(
            self.total_samples + PERIOD_FRAMES as u64 - start,
            RATE,
            self.bpm,
        );
        let mut p = b0.ceil() as u64; // first beat position reached this period
        while (p as f64) < b1 {
            // Apply a pending scene launch at its quantized boundary.
            if let Some(i) = self.transport.tick(p) {
                self.live.active_scene.store(i as u8 + 1, Ordering::Relaxed);
                apply_scene_voice(synth, &self.project, i);
            }
            // Metronome (D16): fires ONLY while recording — never during
            // plain playback, never captured.
            if self.recording.is_some() {
                // Accent the downbeat; kick self-decays.
                let vel: i32 = if p.is_multiple_of(4) { 110 } else { 70 };
                synth.note_on(TICK_CH, 36, vel);
                synth.note_off(TICK_CH, 36);
            }
            p += 1;
        }

        // Pattern playback: 16th-gated step notes for the active scene.
        if let Some(scene) = self.transport.active {
            let s0 = clock::steps_elapsed(self.total_samples - start, RATE, self.bpm);
            let s1 = clock::steps_elapsed(
                self.total_samples + PERIOD_FRAMES as u64 - start,
                RATE,
                self.bpm,
            );
            let (first, last) = Transport::step_window(s0, s1);
            for k in first..last {
                for (t, track) in self.project.tracks.iter().enumerate() {
                    if let Some(Some(clip)) = track.clips.get(scene) {
                        let len = clip.pattern.steps.len() as u64;
                        // 16th gating: off the previous step's note, on this
                        // one (sustains = repeated pitches). saturating: k==0
                        // when a launch starts the clock mid-period (used to
                        // underflow).
                        if let Some(n) = clip.pattern.steps[(k.saturating_sub(1) % len) as usize] {
                            synth.note_off(t as i32, n.pitch as i32);
                        }
                        if let Some(n) = clip.pattern.steps[(k % len) as usize] {
                            synth.note_on(t as i32, n.pitch as i32, n.velocity as i32);
                        }
                    }
                }
            }
        }

        // Take playback (D16 + D18 overdub): every unmuted finalized take
        // loops together — and keeps looping while a NEW take is being
        // captured, so you can layer. Capture only sees the input event
        // lane, never these direct synth calls: no feedback loop.
        let ts0 = clock::steps_elapsed(self.total_samples - start, RATE, self.bpm);
        let ts1 = clock::steps_elapsed(
            self.total_samples + PERIOD_FRAMES as u64 - start,
            RATE,
            self.bpm,
        );
        let mut k = ts0.ceil() as u64;
        while (k as f64) < ts1 {
            for take in &self.takes {
                if take.muted {
                    continue; // kept but silent (take mgmt)
                }
                let lt = take.loop_ticks(self.project.loop_bars) as u64;
                for ev in take.events_at((k % lt) as u32) {
                    match *ev {
                        RecordedEvent::NoteOn {
                            channel,
                            pitch,
                            velocity,
                            ..
                        } => {
                            // Same retrigger semantics on take replay as
                            // live input: cuts last loop's still-ringing
                            // strikes.
                            if is_pad_channel(channel as i32) {
                                synth.all_sounds_off(channel as i32);
                            } else {
                                synth.note_off(channel as i32, pitch as i32);
                            }
                            synth.note_on(channel as i32, pitch as i32, velocity as i32)
                        }
                        RecordedEvent::NoteOff { channel, pitch, .. } => {
                            synth.note_off(channel as i32, pitch as i32)
                        }
                        RecordedEvent::ProgramChange {
                            channel, program, ..
                        } => synth.program_change(channel as i32, program as i32),
                    }
                }
            }
            k += 1;
        }

        self.live.playing.store(true, Ordering::Relaxed);
        self.live
            .pos_beats_x1000
            .store((b1 * 1000.0) as u32, Ordering::Relaxed);
    }
}

// =========================================================== harness tests

#[cfg(test)]
mod harness_tests {
    use super::*;
    use pocketstage_model::{Clip, Pattern, ProjectFile, SCHEMA_VERSION, STEPS_PER_BAR, StepNote};
    use std::sync::mpsc::channel;

    // ---- fake backend ----------------------------------------------------

    #[derive(Debug, PartialEq, Clone, Copy)]
    enum Call {
        On(i32, i32, i32),
        Off(i32, i32),
        Prog(i32, i32),
        Cc(i32, i32, i32),
        NotesOff(i32),
        SoundsOff(i32),
        Fx(f64, f64),
    }

    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
    }

    impl SynthBackend for Fake {
        fn note_on(&mut self, c: i32, k: i32, v: i32) {
            self.calls.push(Call::On(c, k, v));
        }
        fn note_off(&mut self, c: i32, k: i32) {
            self.calls.push(Call::Off(c, k));
        }
        fn program_change(&mut self, c: i32, p: i32) {
            self.calls.push(Call::Prog(c, p));
        }
        fn cc(&mut self, c: i32, t: i32, v: i32) {
            self.calls.push(Call::Cc(c, t, v));
        }
        fn all_notes_off(&mut self, c: i32) {
            self.calls.push(Call::NotesOff(c));
        }
        fn all_sounds_off(&mut self, c: i32) {
            self.calls.push(Call::SoundsOff(c));
        }
        fn render(&mut self, frames: usize, l: &mut [f32], r: &mut [f32]) {
            l[..frames].fill(0.0);
            r[..frames].fill(0.0);
        }
        fn set_global_fx(&mut self, reverb: f64, chorus: f64) {
            self.calls.push(Call::Fx(reverb, chorus));
        }
    }

    // ---- harness ----------------------------------------------------------

    /// bpm where one beat is an exact whole number of periods: 60/125 × 48k
    /// = 23040 frames = 90 periods. Timing assertions are then precise.
    const BPM: f64 = 125.0;
    const PERIODS_PER_BEAT: usize = 90;

    struct Harness {
        core: EngineCore,
        fake: Fake,
        tx_ev: Sender<TimedEvent>,
        rx_ev: Receiver<TimedEvent>,
        tx_cmd: Sender<EngineCommand>,
        rx_cmd: Receiver<EngineCommand>,
        takes_rx: Receiver<Take>,
    }

    impl Harness {
        fn new() -> Self {
            let mut project = Project::default_project();
            project.bpm = BPM;
            let live = Arc::new(LiveStatus::default());
            let stats = Arc::new(Stats {
                live: Arc::clone(&live),
                ..Default::default()
            });
            let (tx_feed, takes_rx) = channel();
            let (tx_ev, rx_ev) = channel();
            let (tx_cmd, rx_cmd) = channel();
            Self {
                core: EngineCore::new(project, live, stats, tx_feed),
                fake: Fake::default(),
                tx_ev,
                rx_ev,
                tx_cmd,
                rx_cmd,
                takes_rx,
            }
        }

        fn send(&self, event: Event) {
            self.tx_ev.send(TimedEvent::now(event)).unwrap();
        }

        /// Run `n` audio periods (each period processes queued events first).
        fn tick(&mut self, n: usize) {
            for _ in 0..n {
                self.core.period(&mut self.fake, &self.rx_cmd, &self.rx_ev);
            }
        }

        fn calls(&mut self) -> Vec<Call> {
            std::mem::take(&mut self.fake.calls)
        }

        fn has(&mut self, c: Call) -> bool {
            self.calls().contains(&c)
        }

        fn project(&self) -> ProjectFile {
            let mut pf = ProjectFile {
                version: SCHEMA_VERSION,
                project: self.core.project.clone(),
            };
            pf.project.takes = self.core.takes.clone();
            pf
        }

        fn clip(name: &str, entries: &[(usize, u8, u8)]) -> Clip {
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
    }

    // ---- transport / scenes -----------------------------------------------

    #[test]
    fn launch_from_stopped_starts_playback_and_pattern() {
        let mut h = Harness::new();
        h.core.project.tracks[0].clips[0] = Some(Harness::clip("P", &[(0, 60, 90)]));
        h.send(Event::LaunchScene { index: 0 });
        h.tick(1);
        assert!(h.core.live.playing.load(Ordering::Relaxed));
        assert_eq!(h.core.live.active_scene.load(Ordering::Relaxed), 1);
        // Step 0 of the pattern sounds in the very first period.
        assert!(h.has(Call::On(0, 60, 90)), "pattern note on launch");
    }

    #[test]
    fn launch_missing_scene_is_ignored_end_to_end() {
        // The live-crash regression (scenes[4] on a 3-scene project), now
        // through the full event pipeline.
        let mut h = Harness::new();
        h.send(Event::LaunchScene { index: 99 });
        h.tick(3);
        assert!(!h.core.live.playing.load(Ordering::Relaxed));
        assert_eq!(h.core.live.active_scene.load(Ordering::Relaxed), 0);
        assert!(h.calls().is_empty(), "ignored launch makes no synth calls");
    }

    #[test]
    fn quantized_switch_fires_exactly_on_the_bar_boundary() {
        let mut h = Harness::new();
        h.core.project.tracks[1].clips[1] = Some(Harness::clip("S2", &[(0, 64, 90)]));
        h.send(Event::Play);
        h.tick(1);
        h.send(Event::LaunchScene { index: 1 }); // NextBar (project default)
        h.tick(1);
        assert_eq!(
            h.core.live.active_scene.load(Ordering::Relaxed),
            0,
            "queued"
        );
        // Playhead started at period boundary; bar = 4 beats = 360 periods.
        h.tick(358);
        assert_eq!(h.core.live.active_scene.load(Ordering::Relaxed), 0);
        h.tick(2);
        assert_eq!(
            h.core.live.active_scene.load(Ordering::Relaxed),
            2,
            "fires on the bar boundary (period 360 at bpm {BPM})"
        );
    }

    #[test]
    fn swap_clamps_pending_launch_after_scene_shrink() {
        // Session-8 live bug: pending S5 fired after an 8→3 shrink.
        let mut h = Harness::new();
        h.send(Event::Play);
        h.tick(1);
        h.send(Event::LaunchScene { index: 4 }); // queued, NextBar
        let mut pf = h.project();
        pf.project.scenes.truncate(3);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(400); // way past the quantize boundary
        assert_eq!(h.core.live.active_scene.load(Ordering::Relaxed), 0);
    }

    // ---- recording ---------------------------------------------------------

    #[test]
    fn record_stop_produces_a_take_on_all_lanes() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.tick(2);
        h.send(Event::StartRecording);
        h.tick(1);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 100,
        });
        h.send(Event::NoteOff { chan: 0, key: 60 });
        h.tick(2);
        h.send(Event::Stop);
        h.tick(1);

        assert_eq!(h.core.takes.len(), 1);
        let take = h.takes_rx.try_recv().expect("take_feed got it");
        assert_eq!(take.name, "Take 1");
        assert_eq!(
            take.events.first(),
            Some(&RecordedEvent::NoteOn {
                tick: 0,
                channel: 0,
                pitch: 60,
                velocity: 100
            })
        );
        assert_eq!(h.core.live.takes.load(Ordering::Relaxed), 1);
        assert!(!h.core.live.recording.load(Ordering::Relaxed));
    }

    #[test]
    fn empty_recording_creates_no_take() {
        let mut h = Harness::new();
        h.send(Event::StartRecording);
        h.tick(2);
        h.send(Event::Stop);
        h.tick(1);
        assert!(h.core.takes.is_empty());
        assert!(h.takes_rx.try_recv().is_err());
    }

    #[test]
    fn stop_recording_finalizes_but_keeps_playback_d15() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.tick(2);
        h.send(Event::StartRecording);
        h.send(Event::NoteOn {
            chan: 0,
            key: 62,
            vel: 90,
        });
        h.tick(1);
        h.send(Event::StopRecording);
        h.tick(1);
        assert_eq!(h.core.takes.len(), 1, "take finalized");
        assert!(
            h.core.live.playing.load(Ordering::Relaxed),
            "playback survives"
        );
        assert!(h.core.playing_since.is_some());
    }

    #[test]
    fn take_plays_back_on_the_loop() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.tick(2);
        h.send(Event::StartRecording);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 100,
        });
        h.tick(2);
        h.send(Event::Stop); // finalize + silent
        h.tick(1);
        h.calls();
        h.send(Event::Play); // take loops from the top
        let mut replays = 0;
        for _ in 0..(PERIODS_PER_BEAT * 8) {
            h.tick(1);
            if h.has(Call::On(0, 60, 100)) {
                replays += 1;
            }
        }
        assert!(replays >= 1, "recorded note replays while looping");
    }

    #[test]
    fn muted_takes_stay_silent() {
        let mut h = Harness::new();
        h.core.takes.push(Take {
            name: "Muted".into(),
            events: vec![RecordedEvent::NoteOn {
                tick: 0,
                channel: 0,
                pitch: 61,
                velocity: 100,
            }],
            muted: true,
        });
        h.send(Event::Play);
        h.tick(200);
        assert!(!h.calls().contains(&Call::On(0, 61, 100)));

        // Unmute via project swap → the note comes back. Loop is 4 bars =
        // 64 steps; tick 0 of the next lap needs ~1500 periods at bpm 125.
        let mut pf = h.project();
        pf.project.takes[0].muted = false;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1500);
        assert!(h.calls().contains(&Call::On(0, 61, 100)));
    }

    #[test]
    fn overdub_captures_only_the_input_lane_d18() {
        let mut h = Harness::new();
        // One take already looping (as in: recorded earlier).
        h.core.takes.push(Take {
            name: "Layer 1".into(),
            events: vec![RecordedEvent::NoteOn {
                tick: 0,
                channel: 2,
                pitch: 50,
                velocity: 70,
            }],
            muted: false,
        });
        h.send(Event::Play);
        h.tick(4);
        h.send(Event::StartRecording); // D18: layer 1 keeps looping
        h.send(Event::NoteOn {
            chan: 0,
            key: 72,
            vel: 111,
        });
        h.tick(4);
        h.send(Event::Stop);
        h.tick(1);
        assert_eq!(h.core.takes.len(), 2);
        let layer2 = h.core.takes[1].events.clone();
        // Exactly the one struck note — none of take 1's replayed output.
        assert_eq!(layer2.len(), 1, "capture is input-lane only");
        assert_eq!(
            layer2[0],
            RecordedEvent::NoteOn {
                tick: 0,
                channel: 0,
                pitch: 72,
                velocity: 111
            }
        );
    }

    // ---- retrigger (D17) ----------------------------------------------------

    #[test]
    fn pad_restrike_kills_via_all_sounds_off_lead_uses_note_off() {
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 10,
            key: 38,
            vel: 100,
        });
        h.tick(1);
        let calls = h.calls();
        assert_eq!(calls[0], Call::SoundsOff(10), "pad channel → CC120");

        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 100,
        });
        h.tick(1);
        let calls = h.calls();
        assert_eq!(calls[0], Call::Off(0, 60), "lead channel → note-off");
    }

    // ---- swaps ---------------------------------------------------------------

    #[test]
    fn swap_keeps_unpersisted_mine_adopts_incoming() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.send(Event::StartRecording);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        h.send(Event::Stop); // "Take 2"? no — first take is Take 1
        h.tick(1);
        assert_eq!(h.core.takes.len(), 1);
        // Incoming store project WITHOUT the just-finalized take (the feed
        // hasn't absorbed it yet) plus an unrelated stored take.
        let mine_name = h.core.takes[0].name.clone();
        let mut pf = h.project();
        pf.project.takes = vec![Take {
            name: "__not_mine__".into(),
            events: vec![],
            muted: false,
        }];
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        let names: Vec<&str> = h.core.takes.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"__not_mine__"), "incoming adopted");
        assert!(
            names.contains(&mine_name.as_str()),
            "un-absorbed mine survives the swap"
        );
    }

    #[test]
    fn swap_applies_mixer_and_program() {
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.tracks[1].gain = Some(0.5);
        pf.project.tracks[1].mute = true;
        pf.project.tracks[1].reverb = Some(1.0);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        let calls = h.calls();
        assert!(calls.contains(&Call::Prog(1, 4)), "track 2 program");
        assert!(calls.contains(&Call::Cc(1, 7, 0)), "mute → CC7=0");
        assert!(calls.contains(&Call::Cc(1, 91, 127)), "reverb send full");
    }

    #[test]
    fn bpm_change_lands_on_swap() {
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.bpm = 180.0;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(h.core.bpm, 180.0);
        assert_eq!(
            h.core.stats.project_updates.load(Ordering::Relaxed),
            1,
            "project_updates counter"
        );
    }

    // ---- drum kit -----------------------------------------------------------------

    #[test]
    fn drum_kit_lands_on_the_feeder_atomic() {
        let mut h = Harness::new();
        // Boot with unset kit → engine default, flagged as written.
        assert_eq!(
            kit_or_default(h.core.stats.drum_kit.load(Ordering::Relaxed)),
            DEFAULT_DRUM_KIT
        );

        let custom: [u8; 8] = [36, 40, 37, 39, 42, 46, 47, 56];
        let mut pf = h.project();
        pf.project.drum_kit = Some(custom);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(
            kit_or_default(h.core.stats.drum_kit.load(Ordering::Relaxed)),
            custom
        );

        // Back to None → default comes back (not "keep last kit").
        let mut pf = h.project();
        pf.project.drum_kit = None;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(
            kit_or_default(h.core.stats.drum_kit.load(Ordering::Relaxed)),
            DEFAULT_DRUM_KIT
        );
    }

    #[test]
    fn kit_packing_round_trips_and_flags() {
        // Never written (atomic default 0) → the fixed default kit.
        assert_eq!(kit_or_default(0), DEFAULT_DRUM_KIT);
        let k: [u8; 8] = [35, 36, 37, 38, 39, 40, 41, 42];
        assert_eq!(unpack_kit(pack_kit(&k)), k);
        assert_eq!(kit_or_default(pack_kit(&k)), k);
    }

    // ---- global fx amounts --------------------------------------------------------

    #[test]
    fn boot_applies_default_global_fx() {
        let mut h = Harness::new();
        h.core.apply_all_voices(&mut h.fake);
        assert!(h.has(Call::Fx(FX_REVERB_DEFAULT, FX_CHORUS_DEFAULT)));
    }

    #[test]
    fn swap_applies_global_fx_knobs() {
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.fx_reverb = Some(0.3);
        pf.project.fx_chorus = Some(1.0);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Fx(0.3, 1.0)));

        // Back to None → engine defaults come back (not "leave as-is").
        let mut pf = h.project();
        pf.project.fx_reverb = None;
        pf.project.fx_chorus = None;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Fx(FX_REVERB_DEFAULT, FX_CHORUS_DEFAULT)));
    }

    // ---- take rename -----------------------------------------------------------

    #[test]
    fn rename_moves_live_copy_and_mine_together() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.send(Event::StartRecording);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        h.send(Event::Stop); // "Take 1", in takes + mine
        h.tick(1);
        assert_eq!(h.core.takes[0].name, "Take 1");

        h.tx_cmd
            .send(EngineCommand::RenameTake {
                from: "Take 1".into(),
                to: "Bass Line".into(),
            })
            .unwrap();
        h.tick(1);
        assert_eq!(h.core.takes[0].name, "Bass Line");
        assert_eq!(h.core.mine[0].0, "Bass Line");

        // The store (already renamed) swaps in with the new name only —
        // a stale mine entry would resurrect "Take 1" alongside it.
        let mut pf = h.project();
        pf.project.takes[0].name = "Bass Line".into();
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(h.core.takes.len(), 1, "no duplicate after rename+swap");
        assert_eq!(h.core.takes[0].name, "Bass Line");
    }

    // ---- misc events ----------------------------------------------------------

    #[test]
    fn cycle_loop_bars_walks_the_ladder() {
        let mut h = Harness::new();
        assert_eq!(h.core.project.loop_bars, 4);
        for expect in [8, 16, 2, 4] {
            h.send(Event::CycleLoopBars);
            h.tick(1);
            assert_eq!(h.core.project.loop_bars, expect);
        }
    }

    #[test]
    fn set_lead_program_hits_channel_zero() {
        let mut h = Harness::new();
        h.send(Event::SetLeadProgram { program: 42 });
        h.tick(1);
        assert!(h.has(Call::Prog(0, 42)));
    }

    #[test]
    fn remote_http_commands_take_the_same_lane() {
        // Web click == key press: EngineCommand variants behave like events.
        let mut h = Harness::new();
        h.tx_cmd.send(EngineCommand::Play).unwrap();
        h.tick(1);
        assert!(h.core.live.playing.load(Ordering::Relaxed));
        h.tx_cmd.send(EngineCommand::LaunchScene(3)).unwrap();
        h.tick(1);
        // Playing already → NextBar queued, not immediate.
        assert_eq!(h.core.live.active_scene.load(Ordering::Relaxed), 0);
        h.tick(PERIODS_PER_BEAT * 4 + 1);
        assert_eq!(h.core.live.active_scene.load(Ordering::Relaxed), 3);
        h.tx_cmd.send(EngineCommand::Stop).unwrap();
        h.tick(1);
        assert!(!h.core.live.playing.load(Ordering::Relaxed));
    }
}
