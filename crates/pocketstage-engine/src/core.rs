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

use pocketstage_model::catalog::{Catalog, PatchRequest, Resolution};
use pocketstage_model::{
    ActiveVoice, Clip, InstrumentSlot, Pattern, Project, ProjectFile, Quantize, RecordedEvent,
    SCHEMA_VERSION, STEPS_PER_BAR, StepNote, Take, Track,
};
use pocketstage_oxygen::KnobTarget;
use pocketstage_server::{EngineCommand, LiveStatus, OverlayTarget, VOICE_DRUMS, VOICE_PATCH};

use crate::clock;
use crate::transport::Transport;

// ---------------------------------------------------------------- constants

pub const RATE: u32 = 48_000;
pub const PERIOD_FRAMES: usize = 256; // frames per period (buffer size knob)

/// D24 write-through debounce: an engine-originated project edit persists
/// after this much quiet (500 ms at the master rate). A fader ride emits a
/// CC storm; only one snapshot leaves, ~500 ms after the last move.
pub const WRITE_THROUGH_QUIET_SAMPLES: u64 = RATE as u64 / 2;

/// Keys play track 1's voice (D3: track N → internal channel N).
pub const LEAD_CH: i32 = 0;
/// D36: normalized starting point for a RELATIVE knob move onto a field that
/// is `None` (soundfont-default). Sends/FX have no readable current value,
/// so the first tick begins from mid-scale — documented, deterministic, and
/// neither silence nor full blast. Gains start at unity instead (their `None`
/// IS defined: 1.0).
pub const REL_NEUTRAL_SEND: f64 = 0.5;
/// MIDI damper ("sustain") CC — measured on the Oxygen pedal, session 16e
/// (`B0 40 7F` / `B0 40 00`). FluidSynth implements the damper per channel
/// itself (threshold ≥64), so the engine only ever forwards it.
pub const CC_DAMPER: i32 = 64;
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

/// Convert one normalized FluidSynth float sample to S16LE.
///
/// `fluid_synth_write_float` emits -1.0..=1.0, so the scale factor is the
/// **i16** full scale. Scaling by `i32::MAX` before clamping to `i16` saturates
/// every sample louder than ~-96 dBFS — i.e. essentially the whole signal —
/// turning the output into a square wave ("crisp" at low levels, buzz/distortion
/// as more samples hit the rails). The clamp is only a safety net for genuine
/// out-of-range float output; it must not normally engage.
#[inline]
fn to_i16(x: f32) -> i16 {
    (x * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Fixed 8-hit GM kit for PRESET-mode pads (D12): kick snare rim clap chh
/// ohh tom crash. Used when the project leaves `drum_kit` unset.
pub const DEFAULT_DRUM_KIT: [u8; 8] = [36, 38, 37, 39, 42, 46, 47, 49];

/// Bank-2 defaults (D25): pads 9–16 extend the kit — deep kick, electric
/// snare, cowbell, crash, low/mid/high tom, gong. Channels mirror bank 1
/// (pad 9 ≙ pad 1: same kill group, different sound — noted trade-off).
pub const DEFAULT_DRUM_KIT_2: [u8; 8] = [35, 40, 56, 57, 45, 48, 50, 55];

/// D21: software overlays lived inside PRESET mode. The MKV's Play/Seq/Mix
/// trims emit nothing the host can see, so SEQ/MIX are entered from controls
/// we own: logical pad 9 toggles MIX, pad 10 toggles SEQ. Mutually exclusive;
/// any DAW/PRESET mode switch exits both. Mirrored to `LiveStatus::overlay`
/// (0 = none, 1 = mix, 2 = seq) for the WS/stats surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Mix,
    Seq,
}

impl Overlay {
    pub fn id(self) -> u8 {
        match self {
            Overlay::None => 0,
            Overlay::Mix => 1,
            Overlay::Seq => 2,
        }
    }
}

/// Overlay-entry/exit voice confirmation (T2: no LEDs — the ears are the
/// display). D21: pad 9 on = kick, off = rim; pad 10 on = tom, off = rim.
/// Distinct channels so a confirmation never cuts a live kit pad.
pub const CONFIRM_KICK: (i32, i32) = (PAD_CH_BASE, 36);
pub const CONFIRM_RIM: (i32, i32) = (PAD_CH_BASE + 1, 37);
pub const CONFIRM_TOM: (i32, i32) = (PAD_CH_BASE + 2, 47);

/// D23 fallback pitch for a step when neither the edited clip nor the track
/// carries a pitch hint (`Track::root_note`).
pub const DEFAULT_STEP_PITCH: u8 = 60;
/// Fixed velocity for pad-toggled SEQ steps (pitch-per-step is deferred in
/// R12; touch dynamics belong to the recorded lane, not the programmer).
pub const STEP_VELOCITY: u8 = 100;

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
    /// Master output gain (`Project::master_gain`), applied at boot and on
    /// every swap. `None` = touch nothing (keep the `synth.gain` the engine
    /// booted with). Pure level — unlike reverb/chorus there are no space
    /// params to stomp; safe to ride live.
    fn set_master_gain(&mut self, gain: Option<f64>);
    /// Global reverb + chorus amount knobs (normalized 0..=1); applied at
    /// boot and on every project swap. `None` means LEAVE THE SYNTH'S
    /// BUILT-IN CONFIG ALONE — do not push any values. Session 10 learned
    /// the hard way that unconditionally overwriting reverb params (stock
    /// room-size here is 0.2; we pushed 0.5) doubles the tail and makes
    /// held notes "stack" (user report); examples/fx_bench.rs has proof.
    /// Chorus `Some(0.0)` explicitly switches the effect off.
    fn set_global_fx(&mut self, reverb: Option<f64>, chorus: Option<f64>);
    /// Select a catalog patch by soundfont NAME on `chan` (D27 phase C —
    /// the `program_select_by_sfont_name` primitive). Callers resolve
    /// against the catalog FIRST; this is only ever invoked with names the
    /// catalog vouched for, so a false return means synth-vs-catalog drift
    /// (logged at the backend, never fatal).
    fn select_patch(&mut self, chan: i32, sfont: &str, bank: i32, program: i32);
}

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
    fn set_master_gain(&mut self, gain: Option<f64>) {
        if let Some(v) = gain {
            self.0.set_gain(v);
        }
    }
    #[inline]
    fn set_global_fx(&mut self, reverb: Option<f64>, chorus: Option<f64>) {
        self.0.set_global_fx(reverb, chorus)
    }
    #[inline]
    fn select_patch(&mut self, chan: i32, sfont: &str, bank: i32, program: i32) {
        if !self.0.select_patch(chan, sfont, bank, program) {
            eprintln!("[patch] synth rejected resolved patch '{sfont}' {bank}:{program} — drift?");
        }
    }
}

// ------------------------------------------------------------------- events

// NOTE: deliberately NOT `Copy` since D27 — `SelectVoice` may carry a boxed
// catalog patch ref. Events move by clone; the lanes are mpsc anyway.
#[derive(Debug, Clone)]
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
    /// D21: pad 9 — toggle the MIX overlay (entering it exits SEQ; toggling
    /// again exits). Voice confirmation is emitted by the core, not the feeder.
    ToggleMixOverlay,
    /// D21: pad 10 — toggle the SEQ overlay (same exclusivity rules).
    ToggleSeqOverlay,
    /// Any DAW/PRESET mode switch exits both overlays (D21).
    ExitOverlays,
    /// Addition B (D26): HTTP-side exact overlay control — `Mix`/`Seq`
    /// enter (idempotent), `None` exits like [`Event::ExitOverlays`].
    /// Never toggles, so a stale view on the web can't flip state by
    /// pressing twice.
    SetOverlay(OverlayTarget),
    /// D22: physical fader move (1-based index, 0..=127). The feeder forwards
    /// these unconditionally; they are only interpreted while MIX is on.
    MixFader {
        index: u8,
        value: u8,
    },
    /// D22: physical knob move (1-based index K1–K8).
    MixKnob {
        index: u8,
        value: u8,
    },
    /// Fader-button press = channel selector (D14, honored by both overlays):
    /// selects the detail page for track n, press again to clear.
    SelectTrack {
        index: u8,
    },
    /// D25: select THE active performance voice (DAW-mode white-key browser).
    /// Persisted into the project (write-through) and mirrored to
    /// `LiveStatus::active_voice` for the feeder + WS.
    SelectVoice {
        voice: ActiveVoice,
    },
    /// D23: bank-1 pad while SEQ is on — toggle step `step` (1–8) of the
    /// current window on the selected track of the active scene.
    SeqStep {
        step: u8,
    },
    /// D36: a raw CC matched by a keymap `knobs[]` row. `target` carries the
    /// param + value mode; `value` is the raw wire value (relative ticks
    /// CW=1/CCW=65 or absolute 0..=127 — resolved in the core, which owns
    /// the current project values). Always live: mapping is data, so no
    /// mode/overlay gate; the ride writes the project and rides the D24
    /// write-through lane exactly like a MIX move.
    KnobParam {
        target: pocketstage_oxygen::KnobTarget,
        value: u8,
    },
    /// Damper pedal (measured CC64, session 16e). Forwarded to the live lead
    /// channel only — pattern tracks keep playing unaffected, and in the drums
    /// voice the keybed is on pad channels, where a damper means nothing.
    /// Deliberately **not recorded into takes**: `RecordedEvent` has no CC
    /// variant, and sustain is a performance gesture around the notes, not part
    /// of them (ruling on backlog item 20; revisit only with a reason).
    Sustain {
        down: bool,
    },
    /// D23: pad 11 — advance the step window one bar within the clip (wraps).
    SeqNextBar,
    /// D23: pad 12 — flip the window between steps 1–8 and 9–16.
    SeqFlipWindow,
}

/// Which mixer field a MIX-overlay control move writes (D22). The CC math
/// mirrors `apply_track_voice` exactly, so a ride and a PUT land the same
/// values on the synth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixField {
    Gain,
    Pan,
    Reverb,
    Chorus,
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
    /// Bank-2 kit (pads 9–16, D25) — same packing as `drum_kit`.
    pub drum_kit_2: AtomicU64,
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

/// Scale (0.0..=1.0) → CC data byte, clamped to the wire range.
///
/// Clamping is not cosmetic: FluidSynth REJECTS a CC outside 0..=127 and a
/// rejected message leaves the channel at its previous value — so an
/// out-of-range field would silently stick the fader at whatever level it held
/// before instead of applying an extreme. The render lane never trusts
/// cross-lane data (D20), so every CC goes through here.
#[inline]
fn to_cc(scale: f64) -> i32 {
    (scale * 127.0).round().clamp(0.0, 127.0) as i32
}

/// Apply a track's voice state — program + mixer — to its internal channel
/// (D3: track N → channel N). Mute is CC7=0; gain 0..=1 maps onto CC7;
/// pan −1..=+1 maps onto CC10. Called at boot and on every project swap, so
/// mixer edits ride the existing PUT → SetProject path with no new plumbing.
///
/// **Gain is an attenuator, not a booster.** CC7 tops out at 127 = unity, so
/// `gain = 1.0` is as loud as this channel can go and there is nowhere above
/// it to go (D33; FluidSynth 2.3.4 has no per-channel float gain — checked:
/// no `fluid_synth_set_channel_gain` symbol). Boost belongs to
/// `Project::master_gain`, which drives the synth's real output stage.
/// Resolve a track's instrument against the catalog (D27 phase C). The
/// patch reference exists only when `soundfont` names one — otherwise the
/// track is plain GM and this returns None (legacy path, untouched).
pub fn track_patch_request(inst: &InstrumentSlot) -> Option<PatchRequest> {
    inst.soundfont.as_ref().map(|sfont| PatchRequest {
        sfont: sfont.clone(),
        bank: inst.bank.unwrap_or(0),
        program: inst.gm_program,
    })
}

/// Apply a track's program selection (GM or catalog patch, D27) to `chan`;
/// fallback notes are appended to `notes` (capped by the caller). Split out
/// so boot, swap and scene-launch all select through ONE code path.
fn apply_track_program<S: SynthBackend>(
    synth: &mut S,
    chan: i32,
    track: &Track,
    cat: &Catalog,
    default_font: &str,
    notes: &mut Vec<String>,
) {
    let outcome = track_patch_request(&track.instrument).map(|req| cat.resolve(req, default_font));
    match outcome {
        Some(Resolution::Resolved(p)) => {
            synth.select_patch(chan, &p.sfont, p.bank as i32, p.program as i32)
        }
        Some(Resolution::FellBack { used, note, .. }) => {
            notes.push(note);
            synth.select_patch(chan, &used.sfont, used.bank as i32, used.program as i32);
        }
        Some(Resolution::Unresolved(req)) => {
            // No font has bank:program — fall back to the plain GM program
            // (which is what this field always meant pre-D27) + badge.
            notes.push(format!(
                "patch '{}' {}:{} unresolved — fell back to GM program {}",
                req.sfont, req.bank, req.program, req.program
            ));
            synth.program_change(chan, req.program as i32);
        }
        None => synth.program_change(chan, track.instrument.gm_program as i32),
    }
}

pub fn apply_track_voice<S: SynthBackend>(
    synth: &mut S,
    chan: i32,
    track: &Track,
    cat: &Catalog,
    default_font: &str,
    notes: &mut Vec<String>,
) {
    apply_track_program(synth, chan, track, cat, default_font, notes);
    let gain = if track.mute {
        0.0
    } else {
        track.gain.unwrap_or(1.0)
    };
    synth.cc(chan, 7, to_cc(gain));
    let pan = ((track.pan.unwrap_or(0.0) + 1.0) * 63.5)
        .round()
        .clamp(0.0, 127.0) as i32;
    synth.cc(chan, 10, pan);
    // FX sends: None = leave the soundfont's own preset value untouched.
    if let Some(v) = track.reverb {
        synth.cc(chan, 91, to_cc(v));
    }
    if let Some(v) = track.chorus {
        synth.cc(chan, 93, to_cc(v));
    }
}

/// Wire value of `LiveStatus::active_voice` for a project (D25): the drums
/// sentinel, an explicit program, or — when unset — track 1's patch (the
/// pre-D25 behaviour: keys simply play track 1).
pub fn voice_code(project: &Project) -> u32 {
    match project.active_voice {
        Some(ActiveVoice::Drums) => VOICE_DRUMS,
        Some(ActiveVoice::Preset(_)) => VOICE_PATCH,
        Some(ActiveVoice::Program(p)) => p as u32,
        None => project
            .tracks
            .first()
            .map(|t| t.instrument.gm_program as u32)
            .unwrap_or(0),
    }
}

/// Voice reset on scene switch: silence every track; (re-)set the program
/// only for tracks that actually have a clip in this column.
///
/// Not damper-aware, deliberately: a scene change does not promise silence, and
/// letting a foot-held chord ring through into the next scene is the musical
/// answer. The paths that DO promise silence (Stop, voice switch) handle the
/// damper explicitly — see `EngineCore::lift_damper`.
pub fn apply_scene_voice<S: SynthBackend>(
    synth: &mut S,
    project: &Project,
    scene: usize,
    cat: &Catalog,
    default_font: &str,
    notes: &mut Vec<String>,
) {
    for (t, track) in project.tracks.iter().enumerate() {
        let chan = project.track_channel(t) as i32;
        synth.all_notes_off(chan);
        if track.clips.get(scene).is_some_and(|c| c.is_some()) {
            apply_track_program(synth, chan, track, cat, default_font, notes);
        }
    }
}

// ----------------------------------------------------------------- the core

/// Per-period state machine of the audio thread. Constructed once and driven
/// exclusively from one thread (the real audio thread or a test loop) —
/// single writer, no locks anywhere in here.
/// Keybed chatter guard (session-10b): under hard sustained pressure the
/// Oxygen keybed bounces — a raw capture shows ~10 ms note-off/note-on
/// re-trigger storms while one key is muscled down. Each re-attack would
/// re-hammer a fresh sample = "stacked" mush. Policy: a release arms a
/// short grace window; a re-strike inside it is swallowed and the note
/// simply keeps sounding. Releases therefore land ~window late (40 ms,
/// imperceptible) — everything else about D17 retrigger semantics is
/// untouched (a strike with NO pending release still resets the voice).
#[derive(Clone, Copy, PartialEq)]
struct KeyGuard {
    /// A note through this engine's live lane is (or was until flush) down.
    down: bool,
    /// `total_samples` when its release was seen; `u64::MAX` = none pending.
    pending_off_at: u64,
}

const KEY_UP: KeyGuard = KeyGuard {
    down: false,
    pending_off_at: u64::MAX,
};

/// 40 ms at 48 kHz — comfortably past the measured ~10 ms chatter period,
/// far below any musical re-strike a human can play per key.
const CHATTER_GUARD_FRAMES: u64 = 48 * 40;

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
    /// D21: active software overlay (None/Mix/Seq); mirrored to the shared
    /// LiveStatus atomic so WS/stats see it without touching core state.
    pub overlay: Overlay,
    /// D22/D23: fader-button channel select — `Some(n)` = detail page for
    /// track n (MIX knobs / SEQ edit target); `None` = 1:1 page mapping
    /// (SEQ then edits track 1).
    pub detail: Option<usize>,
    /// D23: SEQ cursor — bar within the edited clip (pad 11 advances, wraps
    /// at use time) and 8-step half-window (pad 12 flips 1–8 ⇄ 9–16).
    pub seq_bar: usize,
    pub seq_half: bool,
    pub total_samples: u64, // frames rendered so far (master clock domain)
    pub playing_since: Option<u64>, // sample index where Play fired
    pub cycles: u64,
    pub live: Arc<LiveStatus>,
    /// D27 phase C: the loaded patch catalog. Immutable after boot (phase E
    /// replaces the whole Arc), read-only in resolution passes — an `Arc`
    /// pointer read adds no lock, so the single-writer rule (R5) stands.
    pub catalog: Arc<Catalog>,
    /// Internal name of the DEFAULT font: the middle rung of every D27
    /// fallback chain. Set once at boot from the loaded synth.
    pub default_font: String,
    pub stats: Arc<Stats>,
    /// Finalized takes flow back to the HTTP layer (recordings survive
    /// reboot); tests keep the receiving end.
    take_feed: Sender<Take>,
    /// D24 write-through lane out: engine-originated project edits (MIX/
    /// SEQ/SelectVoice) mutate `self.project` immediately, then one
    /// debounced full snapshot goes to the HTTP layer for persistence.
    /// Owned snapshots outbound only — nothing comes back (R5).
    edit_feed: Sender<ProjectFile>,
    /// `total_samples` at the most recent engine-originated project edit;
    /// `Some` = a write-through snapshot is pending the quiet window.
    edit_pending_at: Option<u64>,
    /// Per-(channel, key) chatter state — flat array, no allocation on the
    /// audio thread.
    key_guard: [[KeyGuard; 128]; 16],
    /// Last damper state as *we* left it on the synth. Needed only to re-arm a
    /// physically-held pedal after a kill path (`kill_lead`) drops it to clear
    /// sustained notes; the release always arrives, so it cannot drift.
    sustain_down: bool,
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
        edit_feed: Sender<ProjectFile>,
        catalog: Arc<Catalog>,
        default_font: String,
    ) -> Self {
        // Takes persisted in the project (earlier sessions) are live from t=0.
        let takes = std::mem::take(&mut project.takes);
        live.takes
            .store(takes.len().min(u8::MAX as usize) as u8, Ordering::Relaxed);
        stats.drum_kit.store(
            pack_kit(&project.drum_kit.unwrap_or(DEFAULT_DRUM_KIT)),
            Ordering::Relaxed,
        );
        stats.drum_kit_2.store(
            pack_kit(&project.drum_kit_2.unwrap_or(DEFAULT_DRUM_KIT_2)),
            Ordering::Relaxed,
        );
        live.active_voice
            .store(voice_code(&project), Ordering::Relaxed);
        Self {
            bpm: project.bpm,
            project,
            takes,
            mine: Vec::new(),
            recording: None,
            transport: Transport::default(),
            overlay: Overlay::None,
            detail: None,
            seq_bar: 0,
            seq_half: false,
            total_samples: 0,
            playing_since: None,
            cycles: 0,
            live,
            catalog,
            default_font,
            stats,
            take_feed,
            edit_feed,
            edit_pending_at: None,
            key_guard: [[KEY_UP; 128]; 16],
            sustain_down: false,
            left: vec![0.0; PERIOD_FRAMES],
            right: vec![0.0; PERIOD_FRAMES],
            out: vec![0; PERIOD_FRAMES * 2],
        }
    }

    /// Boot-time: apply every track's program + mixer to its channel,
    /// plus the global FX amounts.
    pub fn apply_all_voices<S: SynthBackend>(&self, synth: &mut S) {
        let mut notes = Vec::new();
        for (t, track) in self.project.tracks.iter().enumerate() {
            let chan = self.project.track_channel(t) as i32;
            apply_track_voice(
                synth,
                chan,
                track,
                &self.catalog,
                &self.default_font,
                &mut notes,
            );
        }
        self.apply_drum_kit(synth, &mut notes);
        self.apply_voice_override(synth, &mut notes);
        self.publish_patch_notes(notes);
        self.apply_global_fx(synth);
    }

    /// D27 phase C: the drum kit as a catalog patch. `None` = do NOTHING —
    /// boot already parked channels 8..=15 on the default font's GM kit.
    /// Otherwise resolve through the kit chain and re-select on every pad
    /// channel (kit NOTE mapping stays GM 35–81 inside, documented).
    fn apply_drum_kit<S: SynthBackend>(&self, synth: &mut S, notes: &mut Vec<String>) {
        let Some(kit) = self.project.drum_kit_patch.as_ref() else {
            return;
        };
        let used = match self
            .catalog
            .resolve_kit(Some(&kit.sfont), &self.default_font)
        {
            Resolution::Resolved(p) => p,
            Resolution::FellBack { used, note, .. } => {
                notes.push(note);
                used
            }
            Resolution::Unresolved(_) => {
                notes.push(format!(
                    "drum kit '{sfont}' unresolved — staying on the boot GM kit",
                    sfont = kit.sfont
                ));
                return;
            }
        };
        for ch in TICK_CH..=15 {
            synth.select_patch(
                ch,
                &used.sfont,
                pocketstage_model::catalog::PERCUSSION_BANK as i32,
                used.program as i32,
            );
        }
    }

    /// Scene launch re-programming with catalog resolution; the pass's
    /// badges replace the previous ones (this is a full resolution pass over
    /// the column, same semantics as boot/swap).
    fn apply_scene_voice_patched<S: SynthBackend>(&self, synth: &mut S, scene: usize) {
        let mut notes = Vec::new();
        apply_scene_voice(
            synth,
            &self.project,
            scene,
            &self.catalog,
            &self.default_font,
            &mut notes,
        );
        self.publish_patch_notes(notes);
    }

    /// Publish this pass's fallback badges for /ws (cap 8 — a project with
    /// more unresolved refs needs its fonts fixed, not a longer list).
    /// Poison-safe by construction: the render loop never panics on this.
    fn publish_patch_notes(&self, notes: Vec<String>) {
        let mut notes = notes;
        notes.truncate(8);
        let mut slot = self
            .live
            .patch_notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = notes;
    }

    /// D25: the selected voice overrides track 1's program on the lead
    /// channel (tracks first, voice wins — call AFTER applying track
    /// voices). Drums needs no synth call; the feeder silences the keybed
    /// and routes notes to the kit. Boot AND swap must layer this: a swap
    /// re-programs channel 0 to track 1's patch, so without it the voice
    /// is silently dropped while /ws still reports it (review P1-1).
    fn apply_voice_override<S: SynthBackend>(&self, synth: &mut S, notes: &mut Vec<String>) {
        if let Some(voice) = self.project.active_voice.as_ref() {
            self.apply_voice_value(synth, voice, notes);
        }
    }

    /// Sound a specific voice on the lead channel (D25 layering + D27 patch
    /// refs). Shared by boot/swap (`apply_voice_override`) and the live
    /// SelectVoice event so resolution semantics cannot diverge between them.
    fn apply_voice_value<S: SynthBackend>(
        &self,
        synth: &mut S,
        voice: &ActiveVoice,
        notes: &mut Vec<String>,
    ) {
        match voice {
            ActiveVoice::Program(p) => synth.program_change(LEAD_CH, *p as i32),
            ActiveVoice::Drums => {}
            ActiveVoice::Preset(req) => {
                match self
                    .catalog
                    .resolve(req.as_ref().clone(), &self.default_font)
                {
                    Resolution::Resolved(p) => {
                        synth.select_patch(LEAD_CH, &p.sfont, p.bank as i32, p.program as i32)
                    }
                    Resolution::FellBack { used, note, .. } => {
                        notes.push(note);
                        synth.select_patch(
                            LEAD_CH,
                            &used.sfont,
                            used.bank as i32,
                            used.program as i32,
                        );
                    }
                    Resolution::Unresolved(r) => {
                        // Lead keeps whatever it currently plays (boot track 1
                        // or the previous voice) — silence would be worse, and
                        // the badge says exactly what did not happen.
                        notes.push(format!(
                            "voice patch '{}' {}:{} unresolved — lead keeps its previous sound",
                            r.sfont, r.bank, r.program
                        ));
                    }
                }
            }
        }
    }

    /// Global output knobs: master gain + FX amounts onto the synth; unset
    /// fields pass `None` = leave the built-in config untouched (see trait
    /// docs). Rides the boot + SetProject paths — no new command needed.
    fn apply_global_fx<S: SynthBackend>(&self, synth: &mut S) {
        synth.set_master_gain(self.project.master_gain.map(|v| v.clamp(0.0, 2.0)));
        synth.set_global_fx(
            self.project.fx_reverb.map(|v| v.clamp(0.0, 1.0)),
            self.project.fx_chorus.map(|v| v.clamp(0.0, 1.0)),
        );
    }

    /// Interleaved S16LE output of the most recent [`Self::period`] — hand
    /// this to ALSA.
    pub fn output(&self) -> &[i16] {
        &self.out
    }

    /// D24: flag that `self.project` just changed by an engine-originated,
    /// user-paced edit (MIX knob, SEQ step, loop-length cycle). The snapshot
    /// leaves once [`WRITE_THROUGH_QUIET_SAMPLES`] pass without a new mark;
    /// every mark refreshes the window, so a ride costs one send.
    pub fn mark_project_edit(&mut self) {
        self.edit_pending_at = Some(self.total_samples);
    }

    /// Full live snapshot in the shape the store speaks (takes included —
    /// the engine owns them; this mirrors what take-feed appends).
    fn snapshot_project(&self) -> ProjectFile {
        let mut pf = ProjectFile {
            version: SCHEMA_VERSION,
            project: self.project.clone(),
        };
        pf.project.takes = self.takes.clone();
        pf
    }

    /// Debounce check, called once per period after the sample counter
    /// advances. The clone + channel send is a documented audio-thread
    /// exception (user-paced, at most one per quiet window — same class as
    /// SetProject's swap). A snapshot racing a web PUT resolves
    /// last-writer-wins on the store; accepted in R12 for mixer values.
    fn maybe_write_through(&mut self) {
        if let Some(at) = self.edit_pending_at
            && self.total_samples - at >= WRITE_THROUGH_QUIET_SAMPLES
        {
            self.edit_pending_at = None;
            // Unbounded send never blocks; a closed lane (server gone) only
            // loses persistence, never audio.
            let _ = self.edit_feed.send(self.snapshot_project());
        }
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
                EngineCommand::SelectVoice(v) => {
                    remote_ev.push_back(TimedEvent::now(Event::SelectVoice { voice: v }));
                }
                EngineCommand::RenameTake { from, to } => self.rename_take(&from, &to),
                EngineCommand::DeleteTake { name } => self.delete_take(&name),
                EngineCommand::SetOverlay(t) => {
                    remote_ev.push_back(TimedEvent::now(Event::SetOverlay(t)));
                }
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

        // 1a) Release any chatter-guarded notes whose grace window elapsed.
        self.flush_guarded_offs(synth);

        // 1b) Master clock: ticks, scene-pattern gating, take loops.
        self.clock_section(synth);

        // 2) Render float L/R -> interleave + scale/clamp to i16.
        let t0 = Instant::now();
        synth.render(PERIOD_FRAMES, &mut self.left, &mut self.right);
        for i in 0..PERIOD_FRAMES {
            self.out[2 * i] = to_i16(self.left[i]);
            self.out[2 * i + 1] = to_i16(self.right[i]);
        }
        let busy = t0.elapsed().as_nanos() as u64;

        self.total_samples += PERIOD_FRAMES as u64;
        self.maybe_write_through();
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
        let mut patch_notes = Vec::new();
        for (t, track) in new.tracks.iter().enumerate() {
            let chan = new.track_channel(t) as i32;
            apply_track_voice(
                synth,
                chan,
                track,
                &self.catalog,
                &self.default_font,
                &mut patch_notes,
            );
        }
        // A PUT may shrink the scene list; drop any dangling active/pending
        // launch before it becomes the project's truth.
        if self.transport.clamp_scenes(new.scenes.len()) {
            self.live.active_scene.store(0, Ordering::Relaxed);
        }
        self.project = new;
        // Tracks are on their channels — re-layer the D25 voice override so
        // the swap cannot silently drop it (review P1-1). Reads the NEW
        // project, hence after the assignment.
        self.apply_voice_override(synth, &mut patch_notes);
        // The chatter table deliberately SURVIVES a swap: it describes notes
        // that are physically sounding on the live lane, and a swap silences
        // nothing. Clearing it (as this used to do) lost both a held key's
        // DOWN flag — the later release then scheduled nothing — and any
        // release already deferred in its grace window. Both stuck a voice
        // until Stop. Rare while PUTs meant "someone clicked Save"; web mixer
        // auto-save makes swaps routine, so the state machine keeps its state
        // (Stop still resets, right before it silences everything).
        self.stats.drum_kit.store(
            pack_kit(&self.project.drum_kit.unwrap_or(DEFAULT_DRUM_KIT)),
            Ordering::Relaxed,
        );
        self.stats.drum_kit_2.store(
            pack_kit(&self.project.drum_kit_2.unwrap_or(DEFAULT_DRUM_KIT_2)),
            Ordering::Relaxed,
        );
        // A swap can change the voice (web PUT edited active_voice).
        let code = voice_code(&self.project);
        self.live.active_voice.store(code, Ordering::Relaxed);
        // Kit may change on a swap too (web edited drum_kit_patch).
        self.apply_drum_kit(synth, &mut patch_notes);
        self.publish_patch_notes(patch_notes);
        self.apply_global_fx(synth);
        self.stats
            .project_updates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Delete a live take (web ✕). Forgetting the `mine` entry matters as much
    /// as dropping the take: the keep-rule re-adds any recently-finalized take
    /// that an incoming document still lists, and this one must not come back
    /// through it. Unknown names are a no-op.
    /// Take the damper off the ledger before killing notes. FluidSynth will not
    /// release anything on a sustained channel, so a kill with the pedal down is
    /// a *silent failure* — and track 1 shares the lead channel, which means
    /// even the per-track loop in Stop is affected. No-ops when no pedal is
    /// down, so the common path emits exactly what it always did.
    fn lift_damper<S: SynthBackend>(&mut self, synth: &mut S) {
        if self.sustain_down {
            synth.cc(LEAD_CH, CC_DAMPER, 0);
        }
    }

    /// Put a still-held pedal back down after a kill. The foot, not the engine,
    /// decides whether notes sustain; every kill path is free to silence the
    /// moment but must not quietly disarm the pedal while it is pressed.
    fn rearm_damper<S: SynthBackend>(&mut self, synth: &mut S) {
        if self.sustain_down {
            synth.cc(LEAD_CH, CC_DAMPER, 127);
        }
    }

    /// Silence the live lead channel with the damper handled around it.
    fn kill_lead<S: SynthBackend>(&mut self, synth: &mut S) {
        self.lift_damper(synth);
        synth.all_notes_off(LEAD_CH);
        self.rearm_damper(synth);
    }

    fn delete_take(&mut self, name: &str) {
        self.takes.retain(|t| t.name != name);
        self.mine.retain(|(n, _)| n != name);
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
            // Unique name: user takes can occupy "Take N" slots, and the
            // model now rejects duplicates (review P2-7) — a colliding name
            // would make the take unpersistable on the drain tick.
            let mut n = self.takes.len() + 1;
            let name = loop {
                let candidate = format!("Take {n}");
                if !self.takes.iter().any(|t| t.name == candidate) {
                    break candidate;
                }
                n += 1;
            };
            let take = Take {
                name,
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
                let t = self.total_samples;
                let slot = self.guard_slot(chan, key);
                if slot.down
                    && slot.pending_off_at != u64::MAX
                    && t - slot.pending_off_at < CHATTER_GUARD_FRAMES
                {
                    // Keybed chatter: cancel the pending release; the note
                    // keeps sounding. No synth call, no capture.
                    slot.pending_off_at = u64::MAX;
                    return;
                }
                slot.down = true;
                slot.pending_off_at = u64::MAX;
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
                let t = self.total_samples;
                let slot = self.guard_slot(chan, key);
                if slot.down && slot.pending_off_at == u64::MAX {
                    // Defer: flush_guarded_offs sounds it out once the grace
                    // window passes without a re-strike. Duplicate offs and
                    // never-cancelled pendings are ignored.
                    slot.pending_off_at = t;
                }
            }
            Event::Sustain { down } => {
                self.sustain_down = down;
                synth.cc(LEAD_CH, CC_DAMPER, if down { 127 } else { 0 });
            }
            Event::Play => {
                if self.playing_since.is_none() {
                    self.playing_since = Some(self.total_samples);
                    self.transport.playing = true;
                }
            }
            Event::Stop => {
                // A release still inside its grace window belongs to the
                // take — capture it before the wholesale silence erases it.
                self.capture_pending_offs();
                // Finalize any live take first (D15: stop means silent;
                // playback waits for Play).
                self.finalize_take();
                self.playing_since = None;
                self.transport.stop();
                self.live.active_scene.store(0, Ordering::Relaxed);
                self.reset_key_guards();
                // Lifted once up front: the loop below can hit this same
                // channel (track 1 routes to it by default), and a kill against
                // a sustained channel does nothing at all.
                self.lift_damper(synth);
                for t in 0..self.project.tracks.len() {
                    let chan = self.project.track_channel(t) as i32;
                    synth.all_notes_off(chan);
                }
                // Keys live on LEAD_CH regardless of track 1's routing
                // override — "stop means silent" must cover them too.
                synth.all_notes_off(LEAD_CH);
                self.rearm_damper(synth);
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
            Event::SelectVoice { voice } => {
                // D25: THE active voice — persisted (write-through), mirrored
                // for the feeder/WS, and sounded: programs re-program the
                // lead channel, Drums answers with a kick (voice-confirm),
                // catalog patches select by name (D27 phase C).
                self.project.active_voice = Some(voice);
                let code = voice_code(&self.project);
                self.live.active_voice.store(code, Ordering::Relaxed);
                // Review P2-4: kill held lead notes BEFORE switching — the
                // feeder re-routes releases by the NEW voice (Drums drops or
                // redirects them), so anything still down would sustain.
                self.kill_lead(synth);
                let mut notes = Vec::new();
                match &self.project.active_voice {
                    Some(ActiveVoice::Drums) => self.confirm(synth, CONFIRM_KICK),
                    Some(v) => self.apply_voice_value(synth, v, &mut notes),
                    None => {}
                }
                if !notes.is_empty() {
                    // One-off selections APPEND to the last full pass's
                    // badges (boot/swap/scene passes replace them wholesale).
                    let mut all = self
                        .live
                        .patch_notes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    all.append(&mut notes);
                    all.truncate(8);
                }
                self.mark_project_edit();
            }
            Event::ToggleMixOverlay => {
                // D21: exclusive — entering MIX exits SEQ; re-press exits.
                let on = self.overlay != Overlay::Mix;
                self.set_overlay(if on { Overlay::Mix } else { Overlay::None });
                self.confirm(synth, if on { CONFIRM_KICK } else { CONFIRM_RIM });
            }
            Event::ToggleSeqOverlay => {
                // Review P2-5: refuse entry while the unit is in DAW mode —
                // the step programmer only routes inside PRESET, so a SEQ
                // overlay armed from DAW would be live but unusable. (An
                // active SEQ never survives the trip INTO DAW either;
                // entering DAW exits overlays.) MIX stays available: its
                // faders route unconditionally.
                if self.live.daw_mode.load(Ordering::Relaxed) {
                    return;
                }
                let on = self.overlay != Overlay::Seq;
                if on {
                    // Every entry starts from a known cursor (bar 1, low half).
                    self.seq_bar = 0;
                    self.seq_half = false;
                }
                self.set_overlay(if on { Overlay::Seq } else { Overlay::None });
                self.confirm(synth, if on { CONFIRM_TOM } else { CONFIRM_RIM });
            }
            Event::SeqStep { step } => {
                // Bank-1 pads 1–8 while SEQ is on (feeder routes them here).
                if self.overlay == Overlay::Seq && (1..=8).contains(&step) {
                    self.seq_toggle_step(step - 1);
                }
            }
            Event::SeqNextBar => {
                if self.overlay == Overlay::Seq {
                    // Wrapping happens at use time (`% clip bars`) so the
                    // cursor stays meaningful when it lands on a 1-bar clip.
                    self.seq_bar = self.seq_bar.wrapping_add(1);
                }
            }
            Event::SeqFlipWindow => {
                if self.overlay == Overlay::Seq {
                    self.seq_half = !self.seq_half;
                }
            }
            Event::ExitOverlays => {
                self.set_overlay(Overlay::None);
                self.detail = None;
            }
            Event::SetOverlay(target) => match target {
                // Anti-brick exact-state entry (D26 Addition B): idempotent,
                // and SEQ keeps the same DAW-mode refusal as the hardware
                // toggle (review P2-5) — an unusable overlay is not an exit.
                OverlayTarget::None => {
                    self.set_overlay(Overlay::None);
                    self.detail = None;
                }
                OverlayTarget::Mix if self.overlay != Overlay::Mix => {
                    self.set_overlay(Overlay::Mix);
                    self.confirm(synth, CONFIRM_KICK);
                }
                OverlayTarget::Seq if self.overlay != Overlay::Seq => {
                    if self.live.daw_mode.load(Ordering::Relaxed) {
                        return;
                    }
                    // Same known-cursor entry as the hardware toggle.
                    self.seq_bar = 0;
                    self.seq_half = false;
                    self.set_overlay(Overlay::Seq);
                    self.confirm(synth, CONFIRM_TOM);
                }
                _ => {} // already in the requested state
            },
            Event::MixFader { index, value } => {
                // D22 page: F(n) → gain of track n. The row used to stop at 4
                // (R2's fake fader-count rationale); per D31 it now follows the
                // project — F5–F8 light up on 5–8-track projects, and a fader
                // addressing an absent track resolves to nothing (mix_set's
                // get_mut guard). Inert outside MIX.
                if self.overlay == Overlay::Mix && self.has_track(index) {
                    self.mix_set(synth, (index - 1) as usize, MixField::Gain, value);
                }
            }
            Event::MixKnob { index, value } => {
                if self.overlay == Overlay::Mix {
                    match self.detail {
                        // Detail page: the knob row K1–K4 becomes ALL four
                        // params of the selected track (D22 leaves the layout
                        // to us; this is the one-page-per-channel reading of
                        // "act on track n alone").
                        Some(t) => {
                            let field = match index {
                                1 => MixField::Gain,
                                2 => MixField::Pan,
                                3 => MixField::Reverb,
                                4 => MixField::Chorus,
                                _ => return,
                            };
                            self.mix_set(synth, t, field, value);
                        }
                        // Default page: K1–K4 → pan of tracks 1–4, K5–K8 →
                        // reverb send of tracks 1–4 (D22). Deliberately still
                        // a four-channel row (D31): the knob inventory is 8,
                        // so it cannot cover 8 channels × 2 fields. Channels
                        // 5–8 are reached through their detail page — press
                        // Fader Button n, then K1–K4 = gain/pan/reverb/chorus.
                        None => {
                            if (1..=4).contains(&index) && self.has_track(index) {
                                self.mix_set(synth, (index - 1) as usize, MixField::Pan, value);
                            } else if (5..=8).contains(&index) && self.has_track(index - 4) {
                                self.mix_set(synth, (index - 5) as usize, MixField::Reverb, value);
                            }
                        }
                    }
                }
            }
            Event::KnobParam { target, value } => self.knob_param(synth, target, value),
            Event::SelectTrack { index } => {
                // Fader buttons are channel selectors, never scene triggers
                // (D14). Press again clears the detail page. Only live inside
                // an overlay (MIX/SEQ) — plain PRESET ignores them. Selectable
                // range follows the project (D31): 8 tracks ⇒ buttons 1–8.
                if self.overlay != Overlay::None && self.has_track(index) {
                    let n = (index - 1) as usize;
                    self.detail = if self.detail == Some(n) {
                        None
                    } else {
                        Some(n)
                    };
                }
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
                        self.apply_scene_voice_patched(synth, scene);
                    }
                    crate::transport::Effect::Switch { scene } => {
                        self.live
                            .active_scene
                            .store(scene as u8 + 1, Ordering::Relaxed);
                        self.apply_scene_voice_patched(synth, scene);
                    }
                    crate::transport::Effect::Queued | crate::transport::Effect::Ignore => {}
                }
            }
        }
    }

    /// Switch the active overlay (mutually exclusive by construction — a
    /// plain assignment) and mirror it to the shared LiveStatus atomic.
    fn set_overlay(&mut self, overlay: Overlay) {
        if overlay == Overlay::None {
            self.detail = None;
        }
        self.overlay = overlay;
        self.live
            .overlay
            .store(overlay.id(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Voice confirmation on a private channel (D21, T2 LED-less UX). The
    /// channel kill keeps rapid toggle-mashing tight — one-shots ignore
    /// note-off (D17 lab), CC120 is the only reliable cut.
    fn confirm<S: SynthBackend>(&self, synth: &mut S, (chan, note): (i32, i32)) {
        synth.all_sounds_off(chan);
        synth.note_on(chan, note, 100);
        synth.note_off(chan, note);
    }

    /// Does a **1-based** control index address a track this project has?
    /// Total + allocation-free; the MIX/overlay gates use it so a fader or
    /// fader-button on an absent channel resolves to nothing (D20) instead of
    /// relying on `mix_set`'s internal guard. D31: this replaced three
    /// hardcoded `(1..=4)` checks.
    fn has_track(&self, index: u8) -> bool {
        index >= 1 && (index as usize) <= self.project.tracks.len()
    }

    /// D23: toggle one step of the 8-step window on the target track of the
    /// active scene (column 1 when nothing plays — no hidden-mode editing).
    /// Toggling into an empty cell creates a 1-bar clip; SEQ never deletes a
    /// clip wholesale. Pitch: the clip's own first pitch, else the track's
    /// `root_note`, else [`DEFAULT_STEP_PITCH`]. No synth call needed: the
    /// clock section re-reads the project every period, so a step lands on
    /// the 16th grid for free (and is instantly audible while looping).
    fn seq_toggle_step(&mut self, idx: u8) {
        let scene = self.transport.active.unwrap_or(0);
        let track = self.detail.unwrap_or(0);
        let Some(tr) = self.project.tracks.get_mut(track) else {
            return;
        };
        let Some(cell) = tr.clips.get_mut(scene) else {
            return;
        };
        if cell.is_none() {
            *cell = Some(Clip {
                name: "SEQ".into(),
                pattern: Pattern {
                    bars: 1,
                    steps: vec![None; STEPS_PER_BAR],
                },
            });
        }
        // D20 discipline: no unwrap in the audio thread — re-check instead.
        if let Some(clip) = cell.as_mut() {
            let bars = usize::from(clip.pattern.bars.max(1));
            let bar = self.seq_bar % bars;
            let abs = bar * STEPS_PER_BAR + if self.seq_half { 8 } else { 0 } + idx as usize;
            if abs >= clip.pattern.steps.len() {
                return; // malformed pattern (validate should prevent): ignore
            }
            if clip.pattern.steps[abs].is_some() {
                clip.pattern.steps[abs] = None; // toggle OFF keeps the clip
            } else {
                let pitch = clip
                    .pattern
                    .steps
                    .iter()
                    .flatten()
                    .map(|n| n.pitch)
                    .next()
                    .or(tr.root_note)
                    .unwrap_or(DEFAULT_STEP_PITCH);
                clip.pattern.steps[abs] = Some(StepNote {
                    pitch,
                    velocity: STEP_VELOCITY,
                });
            }
            self.mark_project_edit();
        }
    }

    /// D22 write-through mix edit: update the project field AND sound it out
    /// immediately (CC math identical to `apply_track_voice`, so a hardware
    /// ride and a web PUT cannot diverge), then flag the lane. Out-of-range
    /// track indices (hostile or stale) resolve to nothing — D20.
    fn mix_set<S: SynthBackend>(
        &mut self,
        synth: &mut S,
        track: usize,
        field: MixField,
        value: u8,
    ) {
        self.mix_set_norm(synth, track, field, value as f64 / 127.0);
    }

    /// Wire CC math for a normalized (0..=1) mix value, shared by the MIX
    /// overlay page and D36 knob bindings: one formula, so a hardware ride,
    /// a knob binding and a web PUT can never diverge.
    fn mix_set_norm<S: SynthBackend>(
        &mut self,
        synth: &mut S,
        track: usize,
        field: MixField,
        v: f64,
    ) {
        let Some(t) = self.project.tracks.get_mut(track) else {
            return;
        };
        let raw = (v * 127.0).round() as i32;
        let (cc, cc_val) = match field {
            // Mute wins over the ride (CC7=0), same rule as apply_track_voice.
            MixField::Gain => {
                t.gain = Some(v);
                (7, if t.mute { 0 } else { raw })
            }
            MixField::Pan => {
                t.pan = Some(v * 2.0 - 1.0);
                (10, raw)
            }
            MixField::Reverb => {
                t.reverb = Some(v);
                (91, raw)
            }
            MixField::Chorus => {
                t.chorus = Some(v);
                (93, raw)
            }
        };
        let chan = self.project.track_channel(track) as i32;
        synth.cc(chan, cc, cc_val);
        self.mark_project_edit();
    }

    /// D36: apply one keymap knob message. Everything lives in the
    /// normalized 0..=1 domain; track fields reuse `mix_set_norm` (identical
    /// CC + model conversion), globals go through the same boot/swap synth
    /// calls (`set_master_gain` / `set_global_fx`, other field left `None`
    /// = touch nothing — the session-10 fx-space rule). Relative moves read
    /// the CURRENT project value, starting from the field's documented
    /// neutral when `None` (gains 1.0, pan center, sends [`REL_NEUTRAL_SEND`]).
    /// Hostile input resolves to nothing: an absent track or index 0 (never
    /// trust cross-lane data, D20) returns before any synth call.
    fn knob_param<S: SynthBackend>(&mut self, synth: &mut S, target: KnobTarget, value: u8) {
        use pocketstage_oxygen::{GlobalParam, KnobParam as Param, TrackField};
        match target.param {
            Param::Track(tt) => {
                // Keymap validation says index >= 1; the render loop does
                // not believe it (D20) — guard before subtracting.
                if tt.index == 0 {
                    return;
                }
                let track = (tt.index - 1) as usize;
                let cur = match self.project.tracks.get(track) {
                    Some(t) => match tt.field {
                        TrackField::Gain => t.gain.unwrap_or(1.0),
                        TrackField::Pan => (t.pan.unwrap_or(0.0) + 1.0) / 2.0,
                        TrackField::Reverb => t.reverb.unwrap_or(REL_NEUTRAL_SEND),
                        TrackField::Chorus => t.chorus.unwrap_or(REL_NEUTRAL_SEND),
                    },
                    None => return,
                }
                .clamp(0.0, 1.0);
                let Some(next) = target.apply(value, cur) else {
                    return;
                };
                let field = match tt.field {
                    TrackField::Gain => MixField::Gain,
                    TrackField::Pan => MixField::Pan,
                    TrackField::Reverb => MixField::Reverb,
                    TrackField::Chorus => MixField::Chorus,
                };
                self.mix_set_norm(synth, track, field, next.clamp(0.0, 1.0));
            }
            Param::Global(gp) => {
                let cur = match gp {
                    GlobalParam::MasterGain => self.project.master_gain.unwrap_or(1.0),
                    GlobalParam::FxReverb => self.project.fx_reverb.unwrap_or(REL_NEUTRAL_SEND),
                    GlobalParam::FxChorus => self.project.fx_chorus.unwrap_or(REL_NEUTRAL_SEND),
                }
                .clamp(0.0, 1.0);
                let Some(next) = target.apply(value, cur) else {
                    return;
                };
                let next = next.clamp(0.0, 1.0);
                match gp {
                    GlobalParam::MasterGain => {
                        self.project.master_gain = Some(next);
                        synth.set_master_gain(Some(next));
                    }
                    GlobalParam::FxReverb => {
                        self.project.fx_reverb = Some(next);
                        synth.set_global_fx(Some(next), None);
                    }
                    GlobalParam::FxChorus => {
                        self.project.fx_chorus = Some(next);
                        synth.set_global_fx(None, Some(next));
                    }
                }
                self.mark_project_edit();
            }
        }
    }

    /// Flat (channel, key) index for the chatter table. Out-of-range
    /// channels/keys are CLAMPED to the table edges — a stray remote event
    /// can never panic the audio thread; at worst it shares an edge slot
    /// with its neighbor (harmless: guards only delay note-offs by 40 ms).
    fn guard_slot(&mut self, chan: i32, key: i32) -> &mut KeyGuard {
        let c = chan.clamp(0, 15) as usize;
        let k = key.clamp(0, 127) as usize;
        &mut self.key_guard[c][k]
    }

    /// Sound out deferred releases whose grace window has elapsed.
    fn flush_guarded_offs<S: SynthBackend>(&mut self, synth: &mut S) {
        let t = self.total_samples;
        for c in 0..16i32 {
            for k in 0..128i32 {
                let flush = {
                    let slot = self.guard_slot(c, k);
                    if slot.down
                        && slot.pending_off_at != u64::MAX
                        && t - slot.pending_off_at >= CHATTER_GUARD_FRAMES
                    {
                        slot.down = false;
                        slot.pending_off_at = u64::MAX;
                        true
                    } else {
                        false
                    }
                };
                if flush {
                    synth.note_off(c, k);
                    self.capture_note_off(c, k);
                }
            }
        }
    }

    /// Stamp any in-flight guarded releases into the live recording.
    fn capture_pending_offs(&mut self) {
        for c in 0..16i32 {
            for k in 0..128i32 {
                let pending = {
                    let slot = self.guard_slot(c, k);
                    slot.down && slot.pending_off_at != u64::MAX
                };
                if pending {
                    self.capture_note_off(c, k);
                }
            }
        }
    }

    /// Forget all guard state (Stop / project swap): the channels are being
    /// silenced wholesale, pending releases must not fire afterwards.
    fn reset_key_guards(&mut self) {
        self.key_guard = [[KEY_UP; 128]; 16];
    }

    /// Deferred half of the NoteOff path — mirrored into a live recording.
    fn capture_note_off(&mut self, chan: i32, key: i32) {
        if let Some((base, evs)) = self.recording.as_mut()
            && let Some(start) = self.playing_since
        {
            let step = clock::steps_elapsed(self.total_samples - start, RATE, self.bpm) as u64;
            evs.push(RecordedEvent::NoteOff {
                tick: step.saturating_sub(*base) as u32,
                channel: chan as u8,
                pitch: key as u8,
            });
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
                self.apply_scene_voice_patched(synth, i);
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
                        // Review P1-2: patterns ride the track's ROUTED
                        // channel (Track.channel override), same as mix_set.
                        let chan = self.project.track_channel(t) as i32;
                        let len = clip.pattern.steps.len() as u64;
                        // 16th gating: off the previous step's note, on this
                        // one (sustains = repeated pitches). saturating: k==0
                        // when a launch starts the clock mid-period (used to
                        // underflow).
                        if let Some(n) = clip.pattern.steps[(k.saturating_sub(1) % len) as usize] {
                            synth.note_off(chan, n.pitch as i32);
                        }
                        if let Some(n) = clip.pattern.steps[(k % len) as usize] {
                            synth.note_on(chan, n.pitch as i32, n.velocity as i32);
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

    #[derive(Debug, PartialEq, Clone)]
    enum Call {
        On(i32, i32, i32),
        Off(i32, i32),
        Prog(i32, i32),
        /// D27: by-name catalog patch selection (chan, sfont, bank, program).
        SelectPatch(i32, String, i32, i32),
        Cc(i32, i32, i32),
        NotesOff(i32),
        SoundsOff(i32),
        Fx(Option<f64>, Option<f64>),
        Gain(Option<f64>),
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
        fn set_master_gain(&mut self, gain: Option<f64>) {
            self.calls.push(Call::Gain(gain));
        }
        fn set_global_fx(&mut self, reverb: Option<f64>, chorus: Option<f64>) {
            self.calls.push(Call::Fx(reverb, chorus));
        }
        fn select_patch(&mut self, c: i32, sfont: &str, bank: i32, program: i32) {
            self.calls
                .push(Call::SelectPatch(c, sfont.to_string(), bank, program));
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
        edit_rx: Receiver<ProjectFile>,
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
            let (tx_edit, edit_rx) = channel();
            let (tx_ev, rx_ev) = channel();
            let (tx_cmd, rx_cmd) = channel();
            Self {
                core: EngineCore::new(
                    project,
                    live,
                    stats,
                    tx_feed,
                    tx_edit,
                    // Empty catalog by default: every patch ref resolves to
                    // "unresolved", so GM fallback behavior is what existing
                    // tests keep asserting. Patch-aware tests install their
                    // own via `h.set_catalog(...)`.
                    Arc::new(Catalog::default()),
                    "DefaultFont".into(),
                ),
                fake: Fake::default(),
                tx_ev,
                rx_ev,
                tx_cmd,
                rx_cmd,
                takes_rx,
                edit_rx,
            }
        }

        /// Swap in a catalog for patch-resolution tests (boot-time stand-in:
        /// the real engine builds it before the core exists).
        fn set_catalog(&mut self, cat: Catalog) {
            self.core.catalog = Arc::new(cat);
        }

        fn send(&self, event: Event) {
            self.tx_ev.send(TimedEvent::now(event)).unwrap();
        }

        /// Grow the stock project to `n` tracks (clips/scenes follow). Used by
        /// the >4-channel MIX tests (D31).
        fn grow_to(&mut self, n: usize) {
            while self.core.project.tracks.len() < n {
                let mut t = self.core.project.tracks[0].clone();
                t.name = format!("Track {}", self.core.project.tracks.len() + 1);
                self.core.project.tracks.push(t);
            }
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

    // ---- S16LE conversion ---------------------------------------------------

    #[test]
    fn float_to_i16_is_linear_until_true_full_scale() {
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i16(0.5), 16_383);
        assert_eq!(to_i16(-0.5), -16_383);
        assert_eq!(to_i16(0.018), 589); // typical live peak — must not saturate
        assert_eq!(to_i16(1.0), i16::MAX);
        assert_eq!(to_i16(-1.0), -i16::MAX);
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(-2.0), i16::MIN);
    }

    // ---- keybed chatter guard (session-10b) ---------------------------------

    #[test]
    fn chatter_storm_collapses_to_one_attack_and_one_release() {
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        assert!(h.has(Call::On(0, 60, 90)));

        // Muscled key: ~10 ms off/on bounce pairs, well inside the 40 ms
        // grace window (a period is ~5.3 ms).
        for _ in 0..3 {
            h.send(Event::NoteOff { chan: 0, key: 60 });
            h.tick(1);
            h.send(Event::NoteOn {
                chan: 0,
                key: 60,
                vel: 85,
            });
            h.tick(1);
        }
        let calls = h.calls(); // the initial attack was consumed by has()
        assert_eq!(
            calls.iter().filter(|c| matches!(**c, Call::On(..))).count(),
            0,
            "storm must not re-attack"
        );
        assert!(!calls.contains(&Call::Off(0, 60)), "no release mid-storm");

        // Final release lands once the grace window passes — not before.
        h.send(Event::NoteOff { chan: 0, key: 60 });
        h.tick(1);
        assert!(!h.has(Call::Off(0, 60)), "release deferred inside grace");
        h.tick(8); // 9 periods ≈ 48 ms > 40 ms
        assert!(h.has(Call::Off(0, 60)), "release lands after grace");
    }

    #[test]
    fn d17_retrigger_survives_the_guard() {
        // A genuine strike with NO pending release still resets the voice.
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 0,
            key: 61,
            vel: 90,
        });
        h.tick(1);
        h.send(Event::NoteOn {
            chan: 0,
            key: 61,
            vel: 95,
        });
        h.tick(1);
        let calls = h.calls();
        assert!(calls.contains(&Call::On(0, 61, 90)));
        assert!(calls.contains(&Call::On(0, 61, 95)));
        assert!(calls.contains(&Call::Off(0, 61)), "D17 reset before re-on");
    }

    #[test]
    fn recording_through_a_storm_captures_one_on_off_pair() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.tick(2);
        h.send(Event::StartRecording);
        h.tick(1);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        for _ in 0..3 {
            h.send(Event::NoteOff { chan: 0, key: 60 });
            h.tick(1);
            h.send(Event::NoteOn {
                chan: 0,
                key: 60,
                vel: 85,
            });
            h.tick(1);
        }
        h.send(Event::NoteOff { chan: 0, key: 60 });
        h.tick(10); // grace expires → guarded off is captured
        h.send(Event::StopRecording);
        h.tick(1);
        assert_eq!(h.core.takes.len(), 1);
        let evs = &h.core.takes[0].events;
        assert_eq!(evs.len(), 2, "exactly one on/off pair, got {evs:?}");
        assert!(matches!(evs[0], RecordedEvent::NoteOn { .. }));
        assert!(matches!(evs[1], RecordedEvent::NoteOff { .. }));
    }

    #[test]
    fn stop_silences_without_ghost_releases_afterwards() {
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        h.calls();
        h.send(Event::Stop);
        h.tick(20);
        assert!(
            !h.calls().contains(&Call::Off(0, 60)),
            "guards reset on Stop; no deferred off fires later"
        );
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
    fn swap_clamps_hostile_mixer_values_into_cc_range() {
        // CC data is 0..=127 and FluidSynth REJECTS values outside that range
        // — and a rejected CC leaves the channel at its PREVIOUS level, so an
        // out-of-range field does not merely fail to apply, it silently sticks
        // at the old value (D32 ear-check: track gain right of center was dead
        // because CC7=203 never landed). Validation caps gain at unity now
        // (D33), but the render lane never trusts cross-lane data (D20):
        // clamp to the edge, never drop the message.
        let mut h = Harness::new();
        {
            let t = &mut h.core.project.tracks[1];
            t.gain = Some(5.0);
            t.pan = Some(-3.0);
            t.reverb = Some(9.0);
            t.chorus = Some(-2.0);
        }
        let pf = h.project();
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        let calls = h.calls();
        assert!(
            calls.contains(&Call::Cc(1, 7, 127)),
            "gain clamped to CC7 max"
        );
        assert!(
            calls.contains(&Call::Cc(1, 10, 0)),
            "pan clamped to CC10 min"
        );
        assert!(
            calls.contains(&Call::Cc(1, 91, 127)),
            "reverb send clamped to CC91 max"
        );
        assert!(
            calls.contains(&Call::Cc(1, 93, 0)),
            "chorus send clamped to CC93 min"
        );
    }

    #[test]
    fn held_note_releases_across_a_project_swap() {
        // Web mixer auto-save turns a fader drag into several swaps while the
        // user is still playing. The swap used to clear the chatter table, so
        // the engine forgot this key was DOWN — the later release found no
        // guard state, scheduled nothing, and the voice stuck until Stop.
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 0,
            key: 62,
            vel: 90,
        });
        h.tick(1);
        assert!(h.has(Call::On(0, 62, 90)));

        h.tx_cmd
            .send(EngineCommand::SetProject(h.project()))
            .unwrap();
        h.tick(1);

        h.send(Event::NoteOff { chan: 0, key: 62 });
        h.tick(1);
        assert!(!h.has(Call::Off(0, 62)), "release still deferred in grace");
        h.tick(8); // past the 40 ms window
        assert!(h.has(Call::Off(0, 62)), "a swap must not swallow releases");
    }

    #[test]
    fn pending_release_survives_a_project_swap() {
        // Same hazard, other ordering: the release is already deferred inside
        // its grace window when the swap lands. Dropping the table deletes a
        // scheduled note-off — an instant stuck note.
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 0,
            key: 63,
            vel: 90,
        });
        h.tick(1);
        h.calls(); // drop the D17 pre-attack release from the attack itself
        h.send(Event::NoteOff { chan: 0, key: 63 });
        h.tick(1);
        assert!(!h.has(Call::Off(0, 63)), "deferred, not sounded yet");

        h.tx_cmd
            .send(EngineCommand::SetProject(h.project()))
            .unwrap();
        h.tick(1);
        h.tick(8);
        assert!(
            h.has(Call::Off(0, 63)),
            "the pending release must still land after a swap"
        );
    }

    #[test]
    fn channel_override_routes_program_notes_and_cc_together() {
        // Review finding P1-2: Track.channel overrides were honored only by
        // mix_set; boot/swap voices, pattern playback and stop used the raw
        // track index — a ch-9 drum track played on 2 and took CCs on 9.
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.tracks[2].channel = Some(11);
        pf.project.tracks[2].clips[0] = Some(Harness::clip("Hit", &[(0, 60, 100)]));
        // Read the program from the project instead of hardcoding it: this test
        // is about ROUTING, and it shouldn't break every time the starter
        // arrangement's instrument choice changes.
        let prog = pf.project.tracks[2].instrument.gm_program as i32;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        let calls = h.calls();
        assert!(
            calls.contains(&Call::Prog(11, prog)),
            "swap programs the OVERRIDDEN channel (track 3 -> ch 11)"
        );
        assert!(
            !calls.contains(&Call::Prog(2, prog)),
            "raw index channel must stay untouched"
        );

        // Scene launch: pattern notes land on the overridden channel too.
        h.send(Event::LaunchScene { index: 0 });
        h.tick(3);
        let calls = h.calls();
        assert!(
            calls.iter().any(|c| matches!(c, Call::On(11, 60, _))),
            "pattern note_on rides the override: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| matches!(c, Call::On(2, 60, _))),
            "no pattern notes on the raw index channel"
        );

        // Stop silences the overridden channel AND the lead channel that
        // keys play on even when track 1 is routed elsewhere.
        h.send(Event::NoteOn {
            chan: LEAD_CH,
            key: 72,
            vel: 90,
        });
        h.tick(1);
        h.calls();
        h.send(Event::Stop);
        h.tick(1);
        let calls = h.calls();
        assert!(calls.contains(&Call::NotesOff(11)), "stop hits ch 11");
        assert!(calls.contains(&Call::NotesOff(0)), "stop hits lead ch 0");
    }

    #[test]
    fn select_voice_silences_held_lead_notes() {
        // Review finding P2-4: held melodic note + switch to Drums — the
        // note-off is filtered out by the drums router, so without a kill
        // the lead voice sustains forever.
        let mut h = Harness::new();
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 100,
        });
        h.tick(1);
        h.send(Event::SelectVoice {
            voice: ActiveVoice::Drums,
        });
        h.tick(1);
        assert!(
            h.calls().contains(&Call::NotesOff(0)),
            "voice change kills held lead notes"
        );
    }

    #[test]
    fn http_set_overlay_drives_exact_state() {
        // D26 Addition B (POST /api/overlay/{mix|seq|none}): exact state,
        // never a toggle — a stale web view pressing twice can't flip it.
        let mut h = Harness::new();
        h.tx_cmd
            .send(EngineCommand::SetOverlay(OverlayTarget::Mix))
            .unwrap();
        h.tick(1);
        assert_eq!(h.core.live.overlay.load(Ordering::Relaxed), 1);
        h.tx_cmd
            .send(EngineCommand::SetOverlay(OverlayTarget::Mix))
            .unwrap();
        h.tick(1);
        assert_eq!(
            h.core.live.overlay.load(Ordering::Relaxed),
            1,
            "re-entering MIX is idempotent"
        );
        h.tx_cmd
            .send(EngineCommand::SetOverlay(OverlayTarget::Seq))
            .unwrap();
        h.tick(1);
        assert_eq!(h.core.live.overlay.load(Ordering::Relaxed), 2);
        // Exit — the anti-brick guarantee — works from any state.
        h.tx_cmd
            .send(EngineCommand::SetOverlay(OverlayTarget::None))
            .unwrap();
        h.tick(1);
        assert_eq!(h.core.live.overlay.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn http_set_overlay_seq_refused_in_daw_mode() {
        // Parity with review P2-5: SEQ entry refuses DAW mode over HTTP
        // exactly like the hardware toggle. MIX + exit stay available —
        // that's the part that must never be blocked.
        let mut h = Harness::new();
        h.core.live.daw_mode.store(true, Ordering::Relaxed);
        h.tx_cmd
            .send(EngineCommand::SetOverlay(OverlayTarget::Seq))
            .unwrap();
        h.tick(1);
        assert_eq!(h.core.live.overlay.load(Ordering::Relaxed), 0);
        h.tx_cmd
            .send(EngineCommand::SetOverlay(OverlayTarget::Mix))
            .unwrap();
        h.tick(1);
        assert_eq!(h.core.live.overlay.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn select_voice_arrives_over_the_http_lane() {
        // Web picker: POST /api/voice → EngineCommand::SelectVoice — the
        // audio thread must run it exactly like the DAW white-key browser's
        // Event (re-program the lead channel + persist via write-through).
        let mut h = Harness::new();
        h.tx_cmd
            .send(EngineCommand::SelectVoice(ActiveVoice::Program(48)))
            .unwrap();
        h.tick(1);
        assert!(
            h.calls().contains(&Call::Prog(0, 48)),
            "lead channel re-programmed"
        );
        assert_eq!(h.core.project.active_voice, Some(ActiveVoice::Program(48)));
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
    fn boot_without_fx_knobs_leaves_synth_stock() {
        // Unset knobs must produce NO synth FX calls — the session-10
        // regression was exactly "boot always overwrites reverb params."
        let mut h = Harness::new();
        h.core.apply_all_voices(&mut h.fake);
        assert!(
            h.has(Call::Fx(None, None)),
            "unset knobs must pass None through"
        );
    }

    #[test]
    fn swap_applies_global_fx_knobs() {
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.fx_reverb = Some(0.3);
        pf.project.fx_chorus = Some(1.0);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Fx(Some(0.3), Some(1.0))));

        // Back to None → leave-as-is, NOT some engine-pushed default.
        let mut pf = h.project();
        pf.project.fx_reverb = None;
        pf.project.fx_chorus = None;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Fx(None, None)));
    }

    // ---- master gain --------------------------------------------------------

    #[test]
    fn boot_without_master_gain_leaves_the_boot_setting() {
        // Unset must pass None through: the engine's synth.gain=0.3 boot
        // default (main.rs) stays in effect.
        let mut h = Harness::new();
        h.core.apply_all_voices(&mut h.fake);
        assert!(h.has(Call::Gain(None)));
    }

    #[test]
    fn master_gain_lands_on_swap_and_clamps() {
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.master_gain = Some(0.6);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Gain(Some(0.6))));

        // Out-of-range input (a hand-edited file that skipped validation)
        // clamps instead of blasting the speakers.
        let mut pf = h.project();
        pf.project.master_gain = Some(9.0);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Gain(Some(2.0))));

        // Back to None -> touch nothing (NOT some engine-pushed default).
        let mut pf = h.project();
        pf.project.master_gain = None;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert!(h.has(Call::Gain(None)));
    }

    // ---- active voice (D25) ----------------------------------------------------

    #[test]
    fn select_program_voice_reprograms_lead_and_persists() {
        let mut h = Harness::new();
        h.send(Event::SelectVoice {
            voice: ActiveVoice::Program(42),
        });
        h.tick(1);
        assert!(h.has(Call::Prog(0, 42)), "lead channel re-programs");
        assert_eq!(
            h.core.live.active_voice.load(Ordering::Relaxed),
            42,
            "feeder/WS mirror"
        );
        h.tick(QUIET_PERIODS);
        let pf = h.edit_rx.try_recv().expect("voice change persisted");
        assert_eq!(pf.project.active_voice, Some(ActiveVoice::Program(42)));
    }

    #[test]
    fn select_drums_voice_confirms_with_kick_and_flags_atomic() {
        let mut h = Harness::new();
        h.send(Event::SelectVoice {
            voice: ActiveVoice::Drums,
        });
        h.tick(1);
        let calls = h.calls();
        assert!(calls.contains(&Call::SoundsOff(8)));
        assert!(calls.contains(&Call::On(8, 36, 100)), "kick = kit armed");
        assert_eq!(
            h.core.live.active_voice.load(Ordering::Relaxed),
            VOICE_DRUMS
        );
    }

    #[test]
    fn boot_applies_selected_program_over_track_one() {
        let mut h = Harness::new();
        h.core.project.active_voice = Some(ActiveVoice::Program(38));
        h.core.apply_all_voices(&mut h.fake);
        let calls = h.calls();
        assert!(
            calls.contains(&Call::Prog(0, 38)),
            "voice wins over track 1's program"
        );
    }

    #[test]
    fn swap_remirrors_active_voice() {
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.active_voice = Some(ActiveVoice::Drums);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(
            h.core.live.active_voice.load(Ordering::Relaxed),
            VOICE_DRUMS
        );
        // Back to unset -> track 1's patch (pre-D25 behaviour).
        let mut pf = h.project();
        pf.project.active_voice = None;
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(h.core.live.active_voice.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn seq_overlay_refused_in_daw_mode() {
        // Review P2-5: the step programmer is only routed in PRESET — a SEQ
        // overlay entered from DAW would be live but unusable. MIX stays
        // available there (faders route unconditionally).
        let mut h = Harness::new();
        h.core.live.daw_mode.store(true, Ordering::Relaxed);
        h.send(Event::ToggleSeqOverlay);
        h.tick(1);
        assert_eq!(
            h.core.live.overlay.load(Ordering::Relaxed),
            0,
            "SEQ must not arm while DAW owns the white keys"
        );
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        assert_eq!(
            h.core.live.overlay.load(Ordering::Relaxed),
            1,
            "MIX still enters from DAW (faders are mode-independent)"
        );
    }

    #[test]
    fn swap_reapplies_voice_program_on_lead() {
        // Review finding P1-1: apply_swap re-programmed channel 0 to track
        // 1's patch and never layered the voice back — keys played piano
        // while /ws reported the selected voice. The last program change
        // on the lead channel must be the voice, not track 1.
        let mut h = Harness::new();
        let mut pf = h.project();
        pf.project.active_voice = Some(ActiveVoice::Program(65));
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        let calls = h.calls();
        let lead_progs: Vec<i32> = calls
            .iter()
            .filter_map(|c| match c {
                Call::Prog(0, p) => Some(*p),
                _ => None,
            })
            .collect();
        assert_eq!(
            lead_progs.last().copied(),
            Some(65),
            "voice wins over track 1 after swap: {lead_progs:?}"
        );
    }

    #[test]
    fn bank2_kit_lands_on_the_feeder_atomic() {
        let mut h = Harness::new();
        assert_eq!(
            kit_or_default(h.core.stats.drum_kit_2.load(Ordering::Relaxed)),
            DEFAULT_DRUM_KIT_2
        );
        let custom: [u8; 8] = [35, 36, 37, 38, 39, 40, 41, 42];
        let mut pf = h.project();
        pf.project.drum_kit_2 = Some(custom);
        h.tx_cmd.send(EngineCommand::SetProject(pf)).unwrap();
        h.tick(1);
        assert_eq!(
            kit_or_default(h.core.stats.drum_kit_2.load(Ordering::Relaxed)),
            custom
        );
    }

    // ---- take rename -----------------------------------------------------------

    #[test]
    fn finalize_skips_occupied_take_names() {
        // Review P2-7 follow-up: the model now rejects duplicate take
        // names, so a finalized take colliding with a user-named one would
        // silently never persist. Finalize must find the next free slot.
        let mut h = Harness::new();
        h.core.takes.push(Take {
            name: "Take 2".into(), // occupies the naive len+1 slot
            events: vec![],
            muted: false,
        });
        h.send(Event::Play);
        h.tick(2);
        h.send(Event::StartRecording);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 100,
        });
        h.tick(1);
        h.send(Event::Stop);
        h.tick(1);
        let names: Vec<&str> = h.core.takes.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"Take 3"),
            "took the next free slot: {names:?}"
        );
        assert_eq!(
            h.takes_rx.try_recv().unwrap().name,
            "Take 3",
            "the persisted copy carries the unique name too"
        );
    }

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

    #[test]
    fn delete_take_stops_playback_and_forgets_mine() {
        let mut h = Harness::new();
        h.send(Event::Play);
        h.send(Event::StartRecording);
        h.send(Event::NoteOn {
            chan: 0,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        h.send(Event::Stop); // "Take 1" lands in takes + mine
        h.tick(1);
        assert_eq!(h.core.takes.len(), 1);
        assert_eq!(h.core.mine.len(), 1);

        h.tx_cmd
            .send(EngineCommand::DeleteTake {
                name: "Take 1".into(),
            })
            .unwrap();
        h.tick(1);
        assert!(h.core.takes.is_empty(), "a deleted take goes silent");
        assert!(
            h.core.mine.is_empty(),
            "and the swap keep-rule must not resurrect it"
        );

        // Unknown names are a no-op (double-click, or two tabs).
        h.tx_cmd
            .send(EngineCommand::DeleteTake {
                name: "nope".into(),
            })
            .unwrap();
        h.tick(1);
        assert!(h.core.takes.is_empty());
    }

    // ---- write-through lane (D24) ------------------------------------------

    /// Periods covering the 500 ms debounce plus slack.
    const QUIET_PERIODS: usize = (WRITE_THROUGH_QUIET_SAMPLES as usize / PERIOD_FRAMES) + 2;

    #[test]
    fn no_engine_edits_means_no_snapshots() {
        let mut h = Harness::new();
        // Live input, playback, even a take — none of it is an engine-
        // originated PROJECT edit (takes ride the take-feed lane instead).
        h.send(Event::Play);
        h.tick(QUIET_PERIODS * 2);
        assert!(h.edit_rx.try_recv().is_err(), "quiet lane stays quiet");
    }

    #[test]
    fn engine_edit_persists_after_the_quiet_window() {
        let mut h = Harness::new();
        h.core.mark_project_edit();
        // 24000 samples / 256 = 93.75 periods — nothing may leave earlier.
        h.tick(93);
        assert!(
            h.edit_rx.try_recv().is_err(),
            "snapshot must wait out the debounce"
        );
        h.tick(1);
        let pf = h.edit_rx.try_recv().expect("snapshot after 500 ms quiet");
        assert_eq!(pf.version, SCHEMA_VERSION);
        assert_eq!(pf.project.bpm, BPM, "carries the live project");
        // One-shot: nothing further until the next edit.
        h.tick(QUIET_PERIODS * 2);
        assert!(h.edit_rx.try_recv().is_err());
    }

    #[test]
    fn continuous_edits_refresh_the_debounce_window() {
        let mut h = Harness::new();
        // A fader ride: marks every 50 periods — always inside the window.
        for _ in 0..10 {
            h.core.mark_project_edit();
            h.tick(50);
            assert!(h.edit_rx.try_recv().is_err(), "ride must not spam");
        }
        // Hands off the fader → one snapshot, then silence.
        h.tick(QUIET_PERIODS);
        assert!(h.edit_rx.try_recv().is_ok());
        h.tick(QUIET_PERIODS * 2);
        assert!(h.edit_rx.try_recv().is_err());
    }

    #[test]
    fn snapshot_carries_live_takes() {
        let mut h = Harness::new();
        h.core.takes.push(Take {
            name: "Layer".into(),
            events: vec![RecordedEvent::NoteOn {
                tick: 0,
                channel: 0,
                pitch: 60,
                velocity: 90,
            }],
            muted: false,
        });
        h.core.mark_project_edit();
        h.tick(QUIET_PERIODS);
        let pf = h.edit_rx.try_recv().expect("snapshot");
        assert_eq!(pf.project.takes.len(), 1, "takes ride along");
        assert_eq!(pf.project.takes[0].name, "Layer");
    }

    // ---- overlays (D21) + MIX (D22) ----------------------------------------

    fn overlay_level(h: &Harness) -> u8 {
        h.core.live.overlay.load(Ordering::Relaxed)
    }

    #[test]
    fn mix_toggle_is_voice_confirmed_kick_on_rim_off() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        assert_eq!(overlay_level(&h), 1);
        let calls = h.calls();
        assert!(calls.contains(&Call::SoundsOff(8)), "kill before confirm");
        assert!(calls.contains(&Call::On(8, 36, 100)), "kick on entry");

        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        assert_eq!(overlay_level(&h), 0);
        assert!(h.has(Call::On(9, 37, 100)), "rim on exit");
    }

    #[test]
    fn overlays_are_mutually_exclusive() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.send(Event::ToggleSeqOverlay); // direct switch mix -> seq
        h.tick(1);
        assert_eq!(overlay_level(&h), 2, "seq replaced mix");
        assert_eq!(h.core.overlay, Overlay::Seq);

        h.send(Event::ToggleSeqOverlay); // toggling seq OFF must NOT revive mix
        h.tick(1);
        assert_eq!(overlay_level(&h), 0);
    }

    #[test]
    fn mode_switch_exits_overlays() {
        // The feeder turns any DAW/PRESET pulse into ExitOverlays (D21).
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.send(Event::SelectTrack { index: 2 });
        h.tick(1);
        assert_eq!(overlay_level(&h), 1);
        h.send(Event::ExitOverlays);
        h.tick(1);
        assert_eq!(overlay_level(&h), 0);
        assert_eq!(h.core.detail, None, "detail page cleared too");
    }

    #[test]
    fn mix_controls_are_inert_without_the_overlay() {
        let mut h = Harness::new();
        h.send(Event::MixFader {
            index: 1,
            value: 90,
        });
        h.send(Event::MixKnob {
            index: 1,
            value: 90,
        });
        h.send(Event::SelectTrack { index: 1 });
        h.tick(1);
        assert!(h.calls().is_empty(), "no synth calls outside MIX");
        assert_eq!(h.core.project.tracks[0].gain, None);
        assert_eq!(h.core.detail, None);
    }

    #[test]
    fn mix_fader_sets_track_gain_and_sounds_it() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.calls();
        h.send(Event::MixFader {
            index: 2,
            value: 64,
        });
        h.tick(1);
        assert_eq!(h.core.project.tracks[1].gain, Some(64.0 / 127.0));
        // Track 2 -> channel 1 (D3); CC7 math matches apply_track_voice.
        assert!(h.has(Call::Cc(1, 7, 64)));
    }

    #[test]
    fn mix_fader_respects_mute() {
        let mut h = Harness::new();
        h.core.project.tracks[1].mute = true;
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.calls();
        h.send(Event::MixFader {
            index: 2,
            value: 100,
        });
        h.tick(1);
        assert_eq!(h.core.project.tracks[1].gain, Some(100.0 / 127.0));
        assert!(
            h.has(Call::Cc(1, 7, 0)),
            "mute keeps the channel silent (CC7=0)"
        );
    }

    #[test]
    fn mix_knob_page_maps_pan_and_reverb() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.calls();
        h.send(Event::MixKnob {
            index: 1,
            value: 127,
        }); // K1 -> tr1 pan
        h.send(Event::MixKnob {
            index: 5,
            value: 100,
        }); // K5 -> tr1 reverb
        h.tick(1);
        assert_eq!(h.core.project.tracks[0].pan, Some(1.0));
        assert_eq!(h.core.project.tracks[0].reverb, Some(100.0 / 127.0));
        // NB: has() consumes the recorded log — check both in one take.
        let calls = h.calls();
        assert!(calls.contains(&Call::Cc(0, 10, 127)));
        assert!(calls.contains(&Call::Cc(0, 91, 100)));
    }

    #[test]
    fn detail_select_redirects_knobs_and_press_again_clears() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.send(Event::SelectTrack { index: 3 });
        h.tick(1);
        assert_eq!(h.core.detail, Some(2));
        h.calls();
        // Detail page: K1 = gain of the SELECTED track only.
        h.send(Event::MixKnob {
            index: 1,
            value: 77,
        });
        h.tick(1);
        assert_eq!(h.core.project.tracks[2].gain, Some(77.0 / 127.0));
        assert_eq!(h.core.project.tracks[0].gain, None, "others untouched");
        assert!(h.has(Call::Cc(2, 7, 77)));
        // K5–K8 have no role on the detail page.
        h.calls();
        h.send(Event::MixKnob {
            index: 6,
            value: 50,
        });
        h.tick(1);
        assert!(h.calls().iter().all(|c| !matches!(c, Call::Cc(..))));
        // Press again clears -> back to the 1:1 page.
        h.send(Event::SelectTrack { index: 3 });
        h.tick(1);
        assert_eq!(h.core.detail, None);
        h.calls();
        h.send(Event::MixKnob {
            index: 1,
            value: 40,
        }); // page: tr1 pan again
        h.tick(1);
        assert!(h.has(Call::Cc(0, 10, 40)));
    }

    #[test]
    fn mix_faders_cover_every_channel_of_an_eight_track_project() {
        // D31: the fader row is bound by the track count, not a hardcoded 4.
        // R12 anticipated exactly this ("F5–F9 unused until more channels
        // exist"). F5 → track 5 gain on channel 4 (D3).
        let mut h = Harness::new();
        h.grow_to(8);
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.calls();
        h.send(Event::MixFader {
            index: 5,
            value: 96,
        });
        h.tick(1);
        assert_eq!(h.core.project.tracks[4].gain, Some(96.0 / 127.0));
        assert!(h.has(Call::Cc(4, 7, 96)));
    }

    #[test]
    fn mix_faders_on_absent_tracks_are_inert() {
        // The stock project has 4 tracks: F5 exists physically but addresses
        // nothing — no synth call, no phantom project edit.
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.calls();
        h.send(Event::MixFader {
            index: 5,
            value: 96,
        });
        h.tick(1);
        assert!(h.calls().is_empty());
        assert!(
            h.edit_rx.try_recv().is_err(),
            "no write-through for nothing"
        );
    }

    #[test]
    fn detail_page_selects_tracks_beyond_four() {
        // Sends/pan for channels 5–8 live on the detail page (D22): F5–F8 are
        // gain, so K1–K4 of the selected track is the way to reach them.
        let mut h = Harness::new();
        h.grow_to(8);
        h.send(Event::ToggleMixOverlay);
        h.send(Event::SelectTrack { index: 7 });
        h.tick(1);
        assert_eq!(h.core.detail, Some(6));
        h.calls();
        h.send(Event::MixKnob {
            index: 4,
            value: 20,
        }); // detail: K4 = chorus
        h.tick(1);
        assert_eq!(h.core.project.tracks[6].chorus, Some(20.0 / 127.0));
        assert!(h.has(Call::Cc(6, 93, 20)));
    }

    #[test]
    fn detail_select_ignores_absent_tracks() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.send(Event::SelectTrack { index: 5 }); // project has 4 tracks
        h.tick(1);
        assert_eq!(
            h.core.detail, None,
            "cannot select a track that isn't there"
        );
    }

    #[test]
    fn out_of_range_mix_controls_are_ignored() {
        // D20: hostile/stale indices resolve to nothing, never a panic.
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.calls();
        for cmd in [
            Event::MixFader {
                index: 9,
                value: 100,
            },
            Event::MixKnob {
                index: 9,
                value: 100,
            },
            Event::SelectTrack { index: 0 },
            Event::SelectTrack { index: 9 },
        ] {
            h.send(cmd);
            h.tick(1);
        }
        assert!(h.calls().is_empty());
        assert_eq!(h.core.detail, None);
    }

    #[test]
    fn mix_edits_ride_the_write_through_lane() {
        let mut h = Harness::new();
        h.send(Event::ToggleMixOverlay);
        h.tick(1);
        h.send(Event::MixFader {
            index: 1,
            value: 33,
        });
        h.tick(QUIET_PERIODS);
        let pf = h.edit_rx.try_recv().expect("mix edit persisted");
        assert_eq!(pf.project.tracks[0].gain, Some(33.0 / 127.0));
    }

    // ---- knob bindings (D36) -------------------------------------------------

    use pocketstage_oxygen::{GlobalParam, KnobKind, KnobParam as KParam, TrackField, TrackTarget};

    fn track_param(index: u8, field: TrackField) -> KParam {
        KParam::Track(TrackTarget { index, field })
    }

    fn knob(kind: KnobKind, step: f64, param: KParam, value: u8) -> Event {
        Event::KnobParam {
            target: KnobTarget { kind, step, param },
            value,
        }
    }

    #[test]
    fn knob_absolute_writes_field_cc_and_write_through() {
        let mut h = Harness::new();
        let chan = h.core.project.track_channel(0) as i32;
        h.send(knob(
            KnobKind::Absolute,
            0.02,
            track_param(1, TrackField::Gain),
            64,
        ));
        h.tick(1);
        assert_eq!(h.core.project.tracks[0].gain, Some(64.0 / 127.0));
        assert!(h.has(Call::Cc(chan, 7, 64)), "CC7 rides with the field");
        h.tick(QUIET_PERIODS);
        let pf = h.edit_rx.try_recv().expect("knob edit persisted");
        assert_eq!(pf.project.tracks[0].gain, Some(64.0 / 127.0));
    }

    #[test]
    fn knob_absolute_mute_wins_over_the_ride() {
        let mut h = Harness::new();
        h.core.project.tracks[0].mute = true;
        let chan = h.core.project.track_channel(0) as i32;
        h.send(knob(
            KnobKind::Absolute,
            0.02,
            track_param(1, TrackField::Gain),
            127,
        ));
        h.tick(1);
        // Field takes the real value; the wire stays silenced (mix_set rule).
        assert_eq!(h.core.project.tracks[0].gain, Some(1.0));
        assert!(h.has(Call::Cc(chan, 7, 0)));
    }

    #[test]
    fn knob_relative_pan_starts_center_and_clamps() {
        let mut h = Harness::new();
        let chan = h.core.project.track_channel(0) as i32;
        let cw = knob(
            KnobKind::Relative,
            0.6,
            track_param(1, TrackField::Pan),
            pocketstage_oxygen::RELATIVE_CW,
        );
        let ccw = knob(
            KnobKind::Relative,
            0.6,
            track_param(1, TrackField::Pan),
            pocketstage_oxygen::RELATIVE_CCW,
        );
        // None → documented neutral (center = normalized 0.5); +0.6 clamps to 1.
        h.send(cw);
        h.tick(1);
        assert_eq!(h.core.project.tracks[0].pan, Some(1.0));
        assert!(h.has(Call::Cc(chan, 10, 127)));
        // One step back reads the CLAMPED value: 1.0 − 0.6 = 0.4 → pan −0.2.
        h.send(ccw.clone());
        h.tick(1);
        assert!((h.core.project.tracks[0].pan.unwrap() + 0.2).abs() < 1e-9);
        // And down here it bottoms out at hard left, never wrapping.
        h.send(ccw.clone());
        h.send(ccw);
        h.tick(1);
        assert_eq!(h.core.project.tracks[0].pan, Some(-1.0));
        assert!(h.has(Call::Cc(chan, 10, 0)));
    }

    #[test]
    fn knob_relative_ignores_unmeasured_wire_values() {
        let mut h = Harness::new();
        h.send(knob(
            KnobKind::Relative,
            0.1,
            track_param(1, TrackField::Gain),
            40, // neither CW(1) nor CCW(65)
        ));
        h.tick(1);
        assert!(h.calls().is_empty(), "ignored tick makes no synth call");
        assert_eq!(h.core.project.tracks[0].gain, None);
        h.tick(QUIET_PERIODS);
        assert!(
            h.edit_rx.try_recv().is_err(),
            "an ignored tick never marks the write-through lane"
        );
    }

    #[test]
    fn knob_absent_track_resolves_to_nothing() {
        let mut h = Harness::new();
        // Index 7 passes keymap validation (≤ MAX_TRACKS) but this project
        // has 4 tracks — hostile/stale data must be inert, never a panic.
        h.send(knob(
            KnobKind::Absolute,
            0.02,
            track_param(7, TrackField::Gain),
            100,
        ));
        // Also index 0: validation rejects it; the core must not subtract.
        h.send(knob(
            KnobKind::Absolute,
            0.02,
            track_param(0, TrackField::Gain),
            100,
        ));
        h.tick(QUIET_PERIODS);
        assert!(h.calls().is_empty());
        assert!(h.edit_rx.try_recv().is_err());
    }

    #[test]
    fn knob_global_master_gain_absolute() {
        let mut h = Harness::new();
        h.send(knob(
            KnobKind::Absolute,
            0.02,
            KParam::Global(GlobalParam::MasterGain),
            32,
        ));
        h.tick(1);
        assert!(h.has(Call::Gain(Some(32.0 / 127.0))));
        assert_eq!(h.core.project.master_gain, Some(32.0 / 127.0));
    }

    #[test]
    fn knob_global_fx_reverb_relative_starts_mid_and_persists() {
        let mut h = Harness::new();
        let cw = pocketstage_oxygen::RELATIVE_CW;
        h.send(knob(
            KnobKind::Relative,
            0.1,
            KParam::Global(GlobalParam::FxReverb),
            cw,
        ));
        h.tick(1);
        // None → mid-scale start, then +step; the chorus slot stays None
        // (session-10 rule: never push params for the untouched effect).
        assert!(h.has(Call::Fx(Some(0.6), None)));
        assert_eq!(h.core.project.fx_reverb, Some(0.6));
        h.tick(QUIET_PERIODS);
        let pf = h.edit_rx.try_recv().expect("fx knob persisted");
        assert_eq!(pf.project.fx_reverb, Some(0.6));
    }

    // ---- SEQ overlay (D23) --------------------------------------------------

    /// Step of track 1's scene-1 clip (window-addressing assertions).
    fn step_at(h: &Harness, i: usize) -> Option<StepNote> {
        h.core.project.tracks[0].clips[0]
            .as_ref()
            .unwrap()
            .pattern
            .steps[i]
    }

    /// Two-bar all-empty clip for window-addressing tests.
    fn empty_two_bar_clip() -> Clip {
        let mut clip = Harness::clip("2bar", &[]);
        clip.pattern.bars = 2;
        clip.pattern.steps.resize(32, None);
        clip
    }

    #[test]
    fn seq_toggle_creates_clip_at_the_right_step() {
        let mut h = Harness::new();
        h.core.project.tracks[0].root_note = Some(48);
        h.send(Event::ToggleSeqOverlay);
        h.tick(1);
        h.calls(); // confirmation tom
        h.send(Event::SeqStep { step: 3 });
        h.tick(1);
        let clip = h.core.project.tracks[0].clips[0]
            .as_ref()
            .expect("empty cell: toggle creates a 1-bar clip");
        assert_eq!(clip.pattern.bars, 1);
        assert_eq!(
            clip.pattern.steps[2],
            Some(StepNote {
                pitch: 48,
                velocity: STEP_VELOCITY
            })
        );
        // No synth calls — audibility comes from the pattern clock, edits
        // quantize to the 16th grid for free.
        assert!(h.calls().iter().all(|c| !matches!(c, Call::On(..))));
    }

    #[test]
    fn seq_toggle_off_clears_the_step_but_keeps_the_clip() {
        let mut h = Harness::new();
        h.send(Event::ToggleSeqOverlay);
        h.send(Event::SeqStep { step: 1 });
        h.tick(1);
        assert!(h.core.project.tracks[0].clips[0].is_some());
        h.send(Event::SeqStep { step: 1 }); // toggle again -> OFF
        h.tick(1);
        let clip = h.core.project.tracks[0].clips[0]
            .as_ref()
            .expect("never deletes wholesale");
        assert_eq!(clip.pattern.steps[0], None);
    }

    #[test]
    fn seq_copies_the_clip_pitch_before_the_root_note() {
        let mut h = Harness::new();
        h.core.project.tracks[0].root_note = Some(48);
        h.core.project.tracks[0].clips[0] = Some(Harness::clip("lead", &[(5, 64, 90)]));
        h.send(Event::ToggleSeqOverlay);
        h.send(Event::SeqStep { step: 1 }); // new step -> copy the clip's own pitch
        h.tick(1);
        let clip = h.core.project.tracks[0].clips[0].as_ref().unwrap();
        assert_eq!(clip.pattern.steps[0].map(|n| n.pitch), Some(64));
    }

    #[test]
    fn seq_window_addressing_bar_and_half_flip() {
        let mut h = Harness::new();
        h.core.project.tracks[0].clips[0] = Some(empty_two_bar_clip());
        h.send(Event::ToggleSeqOverlay);
        h.tick(1);
        // Cursor enters at bar 1, steps 1–8.
        h.send(Event::SeqStep { step: 1 });
        h.tick(1);
        assert!(step_at(&h, 0).is_some(), "bar 1, low half -> abs 0");

        h.send(Event::SeqFlipWindow); // pad 12: steps 9–16
        h.send(Event::SeqStep { step: 2 });
        h.tick(1);
        assert!(step_at(&h, 9).is_some(), "bar 1, high half -> abs 9");

        h.send(Event::SeqNextBar); // pad 11: bar 2 (high half stays flipped)
        h.send(Event::SeqStep { step: 3 });
        h.tick(1);
        assert!(step_at(&h, 26).is_some(), "bar 2, high half -> abs 26");
    }

    #[test]
    fn seq_next_bar_wraps_within_a_one_bar_clip() {
        let mut h = Harness::new();
        h.send(Event::ToggleSeqOverlay);
        for _ in 0..5 {
            h.send(Event::SeqNextBar);
        }
        h.send(Event::SeqStep { step: 1 });
        h.tick(1);
        // Cursor wrapped to the only bar there is — no panic, note landed.
        let clip = h.core.project.tracks[0].clips[0].as_ref().expect("clip");
        assert!(clip.pattern.steps[0].is_some());
    }

    #[test]
    fn seq_targets_the_active_scene_column_or_first_when_stopped() {
        let mut h = Harness::new();
        // Stopped: edits aim at clip column 1 (no hidden-mode editing).
        h.send(Event::ToggleSeqOverlay);
        h.send(Event::SeqStep { step: 1 });
        h.tick(1);
        assert!(h.core.project.tracks[0].clips[0].is_some());

        // Scene 3 launched -> column 3 is the target.
        h.send(Event::LaunchScene { index: 2 });
        h.tick(1);
        h.send(Event::SeqStep { step: 2 });
        h.tick(1);
        assert!(h.core.project.tracks[0].clips[2].is_some(), "active column");
        assert_eq!(
            h.core.project.tracks[0].clips[0]
                .as_ref()
                .unwrap()
                .pattern
                .steps[1],
            None,
            "column 1 untouched once a scene is live"
        );
    }

    #[test]
    fn seq_detail_select_moves_the_edit_target() {
        let mut h = Harness::new();
        h.send(Event::ToggleSeqOverlay);
        h.send(Event::SelectTrack { index: 3 }); // fader button = channel select
        h.send(Event::SeqStep { step: 1 });
        h.tick(1);
        assert!(
            h.core.project.tracks[2].clips[0].is_some(),
            "track 3 edited"
        );
        assert!(
            h.core.project.tracks[0].clips[0].is_none(),
            "track 1 untouched"
        );
    }

    #[test]
    fn seq_entry_resets_the_cursor() {
        let mut h = Harness::new();
        h.send(Event::ToggleSeqOverlay);
        h.send(Event::SeqFlipWindow);
        h.send(Event::SeqNextBar);
        h.send(Event::ToggleSeqOverlay); // exit
        h.send(Event::ToggleSeqOverlay); // re-enter -> bar 1, low half again
        h.send(Event::SeqStep { step: 1 });
        h.tick(1);
        let clip = h.core.project.tracks[0].clips[0].as_ref().expect("clip");
        assert!(clip.pattern.steps[0].is_some(), "cursor reset on entry");
    }

    #[test]
    fn seq_edit_while_looping_becomes_audible_on_the_grid() {
        let mut h = Harness::new();
        h.send(Event::LaunchScene { index: 0 }); // from stopped: plays now
        h.tick(1);
        h.calls();
        h.send(Event::ToggleSeqOverlay);
        h.tick(1);
        h.calls();
        h.send(Event::SeqStep { step: 1 }); // step 0 of the fresh clip
        // One full pattern lap (bar = 360 periods at bpm 125) must replay it.
        h.tick(PERIODS_PER_BEAT * 4 + 10);
        assert!(
            h.calls().contains(&Call::On(
                0,
                DEFAULT_STEP_PITCH as i32,
                STEP_VELOCITY as i32
            )),
            "painted step plays on the 16th grid while looping"
        );
    }

    #[test]
    fn seq_events_are_inert_outside_the_overlay() {
        let mut h = Harness::new();
        // Snapshot rather than "all none": the starter project ships a real
        // arrangement, so an empty grid is no longer the baseline — and "these
        // events changed nothing" is the claim being tested anyway.
        let before = h.core.project.tracks[0].clips.clone();
        h.send(Event::SeqStep { step: 1 });
        h.send(Event::SeqNextBar);
        h.send(Event::SeqFlipWindow);
        h.send(Event::SeqStep { step: 0 }); // out of range even inside
        h.send(Event::SeqStep { step: 9 });
        h.tick(1);
        assert!(h.calls().is_empty());
        assert_eq!(
            h.core.project.tracks[0].clips, before,
            "seq events outside the overlay must not touch clips"
        );
    }

    #[test]
    fn seq_edits_ride_the_write_through_lane() {
        let mut h = Harness::new();
        h.send(Event::ToggleSeqOverlay);
        h.send(Event::SeqStep { step: 4 });
        h.tick(QUIET_PERIODS);
        let pf = h.edit_rx.try_recv().expect("step edit persisted");
        let clip = pf.project.tracks[0].clips[0]
            .as_ref()
            .expect("clip in snapshot");
        assert!(clip.pattern.steps[3].is_some());
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

    // ---- sustain pedal (measured CC64, session 16e) ----------------------

    #[test]
    fn sustain_pedal_forwards_damper_to_the_lead_channel() {
        let mut h = Harness::new();
        h.send(Event::Sustain { down: true });
        h.tick(1);
        assert_eq!(h.calls(), vec![Call::Cc(LEAD_CH, CC_DAMPER, 127)]);
        h.send(Event::Sustain { down: false });
        h.tick(1);
        assert_eq!(h.calls(), vec![Call::Cc(LEAD_CH, CC_DAMPER, 0)]);
    }

    #[test]
    fn stop_without_the_pedal_emits_no_damper_traffic() {
        // The no-pedal path stays byte-for-byte what it was before a pedal
        // existed — several tests assert exact call lists.
        let mut h = Harness::new();
        h.send(Event::Stop);
        h.tick(1);
        assert!(
            !h.calls()
                .iter()
                .any(|c| matches!(c, Call::Cc(_, CC_DAMPER, _))),
            "no damper CC unless a pedal is down"
        );
    }

    #[test]
    fn stop_with_pedal_held_lifts_damper_kills_then_rearms() {
        let mut h = Harness::new();
        h.send(Event::Sustain { down: true });
        h.send(Event::NoteOn {
            chan: LEAD_CH,
            key: 60,
            vel: 90,
        });
        h.tick(1);
        h.calls(); // drain the pedal + strike
        h.send(Event::Stop);
        h.tick(1);
        let calls = h.calls();
        let idx = |want: Call| calls.iter().position(|c| *c == want);
        let offs: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == Call::NotesOff(LEAD_CH))
            .map(|(i, _)| i)
            .collect();
        // Track 1 routes to the lead channel by default, so Stop kills it more
        // than once; what matters is that EVERY kill sits between the lift and
        // the re-arm. A kill outside that window happens against a sustained
        // channel and silently does nothing (held keys would keep sounding).
        let lifted = idx(Call::Cc(LEAD_CH, CC_DAMPER, 0)).expect("damper lifted");
        let rearmed = idx(Call::Cc(LEAD_CH, CC_DAMPER, 127)).expect("held pedal re-armed");
        assert!(!offs.is_empty(), "stop silenced the lead channel");
        assert!(
            offs.iter().all(|i| *i > lifted && *i < rearmed),
            "every lead kill must sit between lift and re-arm: {calls:?}"
        );
    }

    #[test]
    fn voice_switch_lifts_the_damper_before_reprogramming_the_channel() {
        let mut h = Harness::new();
        h.send(Event::Sustain { down: true });
        h.tick(1);
        h.calls();
        h.send(Event::SelectVoice {
            voice: ActiveVoice::Program(4),
        });
        h.tick(1);
        let calls = h.calls();
        let lead: Vec<Call> = calls
            .into_iter()
            .filter(|c| match c {
                Call::Cc(ch, _, _) | Call::NotesOff(ch) | Call::Prog(ch, _) => *ch == LEAD_CH,
                _ => false,
            })
            .collect();
        assert_eq!(
            lead,
            vec![
                Call::Cc(LEAD_CH, CC_DAMPER, 0),
                Call::NotesOff(LEAD_CH),
                Call::Cc(LEAD_CH, CC_DAMPER, 127),
                Call::Prog(LEAD_CH, 4),
            ],
            "held notes die with the damper lifted, THEN the new program lands"
        );
    }

    #[test]
    fn pedal_presses_are_not_recorded_into_takes() {
        // Ruling (backlog item 20): sustain frames the notes; it is not part of
        // them. `RecordedEvent` has no CC variant, so this asserts the shape we
        // chose rather than leaving it implicit.
        let mut h = Harness::new();
        h.send(Event::StartRecording);
        h.send(Event::Sustain { down: true });
        h.send(Event::NoteOn {
            chan: LEAD_CH,
            key: 60,
            vel: 90,
        });
        h.tick(2);
        h.send(Event::Sustain { down: false });
        h.send(Event::NoteOff {
            chan: LEAD_CH,
            key: 60,
        });
        h.send(Event::Stop);
        h.tick(1);
        let take = h.takes_rx.try_recv().expect("stop finalized a take");
        assert_eq!(
            take.events.len(),
            2,
            "note-on + note-off only, no pedal events: {:?}",
            take.events
        );
    }

    // ============================================ D27 phase C: patch refs

    /// Three-font test catalog. "DefaultFont" matches the harness core's
    /// `default_font`, so default-rung fallbacks resolve in tests exactly as
    /// they would against the boot font.
    fn patch_catalog() -> Catalog {
        Catalog::build(vec![
            (
                "DefaultFont".into(),
                "/f/d.sf2".into(),
                vec![(0, 1, "Piano".into()), (128, 0, "GM Kit".into())],
            ),
            (
                "Melodic".into(),
                "/f/m.sf2".into(),
                vec![(8, 33, "Bass Finger".into())],
            ),
            (
                "KitFont".into(),
                "/f/k.sf2".into(),
                vec![(128, 0, "Tight Kit".into())],
            ),
        ])
    }

    fn patch_notes(h: &Harness) -> Vec<String> {
        h.core
            .live
            .patch_notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[test]
    fn track_with_soundfont_selects_by_name_not_program_change() {
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        let t = &mut h.core.project.tracks[0];
        t.instrument.soundfont = Some("Melodic".into());
        t.instrument.bank = Some(8);
        t.instrument.gm_program = 33;
        h.core.apply_all_voices(&mut h.fake);
        // Snapshot ONCE — `h.has()` drains the log (destructive by design).
        let calls = h.calls();
        assert!(calls.contains(&Call::SelectPatch(0, "Melodic".into(), 8, 33)));
        // The legacy GM program_change must be GONE for this channel — both
        // firing would silently re-select on the default font afterwards.
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Prog(0, _))),
            "got {calls:?}"
        );
        assert!(patch_notes(&h).is_empty(), "exact hit leaves no badge");
    }

    #[test]
    fn track_patch_falls_back_to_default_font_with_visible_note() {
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        let t = &mut h.core.project.tracks[0];
        t.instrument.soundfont = Some("Nope".into());
        t.instrument.gm_program = 1; // exists on DefaultFont bank 0
        h.core.apply_all_voices(&mut h.fake);
        let calls = h.calls();
        assert!(calls.contains(&Call::SelectPatch(0, "DefaultFont".into(), 0, 1)));
        let notes = patch_notes(&h);
        assert_eq!(notes.len(), 1, "got {notes:?}");
        assert!(notes[0].contains("default font"), "got {}", notes[0]);
    }

    #[test]
    fn unresolved_track_patch_falls_back_to_gm_and_keeps_its_ccs() {
        // Q-R2 pinned: MIX overlay on an unresolved channel still works —
        // CCs are font-independent, resolution only touches the program.
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        let t = &mut h.core.project.tracks[0];
        t.instrument.soundfont = Some("Ghost".into());
        t.instrument.bank = Some(7);
        t.instrument.gm_program = 77; // nowhere in the catalog
        h.core.apply_all_voices(&mut h.fake);
        let calls = h.calls();
        assert!(calls.contains(&Call::Prog(0, 77)), "got {calls:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::SelectPatch(..))));
        // ...and the whole mixer block still lands (CC7 gain is applied by
        // apply_track_voice right after the program line).
        assert!(calls.contains(&Call::Cc(0, 7, 127)), "got {calls:?}");
        let notes = patch_notes(&h);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("unresolved"), "got {}", notes[0]);
    }

    #[test]
    fn select_voice_preset_selects_by_name_and_mirrors_ws_code() {
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        h.send(Event::SelectVoice {
            voice: ActiveVoice::Preset(Box::new(pocketstage_model::PatchRef {
                sfont: "Melodic".into(),
                bank: 8,
                program: 33,
            })),
        });
        h.tick(1);
        let calls = h.calls();
        assert!(calls.contains(&Call::SelectPatch(LEAD_CH, "Melodic".into(), 8, 33)));
        assert_eq!(
            h.core.live.active_voice.load(Ordering::Relaxed),
            VOICE_PATCH
        );
        assert!(matches!(
            h.core.project.active_voice,
            Some(ActiveVoice::Preset(_))
        ));
        assert!(patch_notes(&h).is_empty());
    }

    #[test]
    fn select_voice_preset_unresolved_keeps_previous_sound_and_badges() {
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        h.send(Event::SelectVoice {
            voice: ActiveVoice::Preset(Box::new(pocketstage_model::PatchRef {
                sfont: "Ghost".into(),
                bank: 7,
                program: 77,
            })),
        });
        h.tick(1);
        let calls = h.calls();
        // Kill happens (P2-4), but NOTHING re-programs the lead channel:
        // it keeps playing what it played before — plus a loud badge.
        assert!(calls.contains(&Call::NotesOff(LEAD_CH)));
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::SelectPatch(..) | Call::Prog(0, _)))
        );
        let notes = patch_notes(&h);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("keeps its previous sound"),
            "got {notes:?}"
        );
    }

    #[test]
    fn drum_kit_patch_reselects_every_pad_channel() {
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        h.core.project.drum_kit_patch = Some(pocketstage_model::PatchRef {
            sfont: "KitFont".into(),
            bank: 128,
            program: 0,
        });
        h.core.apply_all_voices(&mut h.fake);
        let calls = h.calls();
        for ch in 8..=15 {
            assert!(
                calls.contains(&Call::SelectPatch(ch, "KitFont".into(), 128, 0)),
                "pad channel {ch} missed the kit: {calls:?}"
            );
        }
        assert!(patch_notes(&h).is_empty());
    }

    #[test]
    fn drum_kit_fallback_walks_to_default_font_kit() {
        let mut h = Harness::new();
        h.set_catalog(patch_catalog());
        h.core.project.drum_kit_patch = Some(pocketstage_model::PatchRef {
            sfont: "Melodic".into(), // loaded, but has NO percussion
            bank: 128,
            program: 0,
        });
        h.core.apply_all_voices(&mut h.fake);
        let calls = h.calls();
        assert!(calls.contains(&Call::SelectPatch(8, "DefaultFont".into(), 128, 0)));
        let notes = patch_notes(&h);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("no percussion"), "got {notes:?}");
    }

    #[test]
    fn drum_kit_unresolved_stays_on_boot_gm_kit() {
        let mut h = Harness::new();
        // Empty catalog: nothing is loaded, no kit exists anywhere.
        h.core.project.drum_kit_patch = Some(pocketstage_model::PatchRef {
            sfont: "Ghost".into(),
            bank: 128,
            program: 0,
        });
        h.core.apply_all_voices(&mut h.fake);
        let calls = h.calls();
        assert!(!calls.iter().any(|c| matches!(c, Call::SelectPatch(..))));
        let notes = patch_notes(&h);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("boot GM kit"), "got {notes:?}");
    }
}
