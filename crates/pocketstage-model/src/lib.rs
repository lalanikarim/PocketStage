//! PocketStage project data model (scratchpad R2).
//!
//! The core structure is the **clip matrix**: tracks × scenes. A clip lives at
//! a `(track, scene)` cell; launching a scene plays that whole column. This is
//! the single source of truth shared by the engine, the Oxygen mapper and the
//! web UI — hence its own crate with zero audio/MIDI dependencies.
//!
//! File format: [`ProjectFile`] wraps a schema `version` so future migrations
//! are explicit. JSON on disk (human-diffable; msgpack can be an *additional*
//! option later, per "open & hackable" differentiator).

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// R2: max 4 tracks (matches Oxygen 49 MKV's 4 faders exactly).
pub const MAX_TRACKS: usize = 4;
/// R2 / MVP: max 8 scenes (triggered by pads 1-8 in PLAY mode).
pub const MAX_SCENES: usize = 8;
/// Pattern resolution is fixed at 16th notes (R2).
pub const STEPS_PER_BAR: usize = 16;
/// R2: pattern length in bars, 1..=4.
pub const MAX_PATTERN_BARS: u8 = 4;
/// Take loop floor in bars (D16): `loop_bars` is the *minimum* loop length —
/// a longer recorded take rounds up to whole bars instead of truncating.
/// Set via the B-key override in DAW mode (R11); range covers sane settings.
pub const LOOP_BARS_RANGE: std::ops::RangeInclusive<u8> = 1..=16;

// ------------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    #[error("unsupported schema version {0}: this build understands version <= {SCHEMA_VERSION}")]
    UnsupportedVersion(u32),
    #[error("invalid project file: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ValidationError {
    #[error("project has {} tracks, max is {MAX_TRACKS}", .0)]
    TooManyTracks(usize),
    #[error("project has {} scenes, max is {MAX_SCENES}", .0)]
    TooManyScenes(usize),
    #[error("need at least 1 scene")]
    NoScenes,
    #[error("track '{track}' has {slots} clip slots but project has {scenes} scenes")]
    ClipSlotMismatch {
        track: String,
        slots: usize,
        scenes: usize,
    },
    #[error("bpm {0} out of range 20.0..=300.0")]
    BpmOutOfRange(f64),
    #[error("pattern length {bars} bars out of range 1..={MAX_PATTERN_BARS}")]
    PatternBarsOutOfRange { bars: u8 },
    #[error(
        "pattern has {} steps, expected {} ({} bars x {STEPS_PER_BAR})",
        .0, .1, .2
    )]
    PatternStepCount(usize, usize, u8),
    #[error("note pitch {0} out of range 0..=127")]
    PitchOutOfRange(u8),
    #[error("velocity {0} out of range 1..=127")]
    VelocityOutOfRange(u8),
    #[error("gm_program {0} out of range 0..=127")]
    ProgramOutOfRange(u8),
    #[error("track '{track}' channel override {channel} out of range 0..=15")]
    ChannelOutOfRange { track: String, channel: u8 },
    #[error(
        "loop_bars {0} out of range {min}..={max}",
        min = LOOP_BARS_RANGE.start(),
        max = LOOP_BARS_RANGE.end()
    )]
    LoopBarsOutOfRange(u8),
    #[error("take '{take}' events not sorted by tick")]
    UnsortedTake { take: String },
    #[error("track '{track}' gain {gain} out of range 0..=2")]
    GainOutOfRange { track: String, gain: f64 },
    #[error("track '{track}' pan {pan} out of range -1..=1")]
    PanOutOfRange { track: String, pan: f64 },
    #[error("track '{track}' fx send {send} out of range 0..=1")]
    SendOutOfRange { track: String, send: f64 },
    #[error("global fx {name} {value} out of range 0..=1")]
    FxOutOfRange { name: &'static str, value: f64 },
    #[error("drum kit note {note} out of range 35..=81 (GM percussion)")]
    DrumNoteOutOfRange { note: u8 },
}

// ---------------------------------------------------------------- top level

/// On-disk envelope. Bump [`SCHEMA_VERSION`] and write a migration when the
/// shape changes incompatibly; unknown-future versions are rejected, never
/// guessed at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub project: Project,
}

impl ProjectFile {
    /// Parse + version-check. Rejects future schema versions explicitly.
    pub fn from_json_str(s: &str) -> Result<Self, ModelError> {
        let value: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ModelError::Invalid(e.to_string()))?;
        let version = match &value {
            serde_json::Value::Object(map) => map
                .get("version")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| ModelError::Invalid("object without numeric 'version'".into()))?,
            other => {
                return Err(ModelError::Invalid(format!(
                    "expected object at top level, got {}",
                    Self::json_type_name(other)
                )));
            }
        };
        if version > SCHEMA_VERSION as u64 {
            return Err(ModelError::UnsupportedVersion(version as u32));
        }
        serde_json::from_value(value).map_err(|e| ModelError::Invalid(e.to_string()))
    }

    fn json_type_name(v: &serde_json::Value) -> &'static str {
        match v {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        }
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Validate the wrapped project.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.project.validate()
    }
}

/// A PocketStage project: fixed tempo, clip matrix, recorded takes.
///
/// Schema note: fields added since v1 (`loop_bars`, `Track::channel`) are
/// optional with defaults — old files parse unchanged and new files stay
/// readable by older builds (unknown fields ignored), so `SCHEMA_VERSION`
/// stays at 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    /// Tempo in BPM. Fixed per project in v0.1 (R2).
    #[serde(default = "default_bpm")]
    pub bpm: f64,
    /// Minimum take loop length in bars (D16). Recorded takes shorter than
    /// this still loop at `loop_bars`; longer ones round up to whole bars.
    #[serde(default = "default_loop_bars")]
    pub loop_bars: u8,
    pub tracks: Vec<Track>,
    /// Vertical slices: launching a scene plays every non-empty clip in that
    /// column (quantized per [`Scene::quantize`], D5).
    pub scenes: Vec<Scene>,
    /// Recorded MIDI performances (D4: MIDI-only recording in v0.1).
    #[serde(default)]
    pub takes: Vec<Take>,
    /// Global reverb level 0..=1 (group-level knob only; room/damp/width
    /// are never written). `None` = leave the synth's built-in FX config
    /// untouched — session 10 learned that forcing params at boot changes
    /// the stock sound ("stacked" held keys). Applied at boot and swap.
    #[serde(default)]
    pub fx_reverb: Option<f64>,
    /// Global chorus level 0..=1 (normalized; maps onto FluidSynth's
    /// native 0..=4 group level). `None` = leave the synth's built-in FX
    /// config untouched; 0 explicitly switches chorus OFF.
    #[serde(default)]
    pub fx_chorus: Option<f64>,
    /// GM drum notes for PRESET-mode pads 1–8 (D12). `None` = the engine's
    /// fixed kit (kick snare rim clap chh ohh tom crash). Each note must be
    /// in the GM percussion range 35..=81; pads keep one channel each.
    #[serde(default)]
    pub drum_kit: Option<[u8; 8]>,
}

fn default_bpm() -> f64 {
    120.0
}

fn default_loop_bars() -> u8 {
    4
}

impl Project {
    /// The stock boot project for the v0.1 vertical slice (R6): 4 tracks with
    /// curated R8 starter patches, 8 empty scenes, 120 BPM.
    pub fn default_project() -> Self {
        let starters: [(&str, u8, [u8; 3]); MAX_TRACKS] = [
            ("Piano", 0, [240, 220, 130]),   // Acoustic Grand
            ("E.Piano", 4, [130, 200, 240]), // EPiano 1
            ("Pad", 89, [190, 140, 240]),    // Pad 2 (warm)
            ("Bass", 33, [140, 240, 150]),   // Finger Bass
        ];
        Project {
            name: "Untitled".into(),
            bpm: default_bpm(),
            loop_bars: default_loop_bars(),
            tracks: starters
                .iter()
                .map(|(name, program, color)| Track {
                    name: (*name).into(),
                    color: *color,
                    instrument: InstrumentSlot {
                        soundfont: None,
                        gm_program: *program,
                    },
                    clips: vec![None; MAX_SCENES],
                    channel: None,
                    gain: None,
                    pan: None,
                    mute: false,
                    reverb: None,
                    chorus: None,
                    root_note: None,
                })
                .collect(),
            scenes: (1..=MAX_SCENES)
                .map(|i| Scene {
                    name: format!("S{i}"),
                    quantize: None,
                })
                .collect(),
            takes: Vec::new(),
            fx_reverb: None,
            fx_chorus: None,
            drum_kit: None,
        }
    }

    /// Internal synth channel for track `index`: the override if set, else
    /// the track index itself (D3). Panics only on out-of-bounds index.
    pub fn track_channel(&self, index: usize) -> u8 {
        self.tracks[index].channel.unwrap_or(index as u8)
    }

    /// Validate all invariants the engine relies on (see HANDOFF/scratchpad R2).
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.tracks.len() > MAX_TRACKS {
            return Err(ValidationError::TooManyTracks(self.tracks.len()));
        }
        if self.scenes.is_empty() {
            return Err(ValidationError::NoScenes);
        }
        if self.scenes.len() > MAX_SCENES {
            return Err(ValidationError::TooManyScenes(self.scenes.len()));
        }
        if !(20.0..=300.0).contains(&self.bpm) {
            return Err(ValidationError::BpmOutOfRange(self.bpm));
        }
        if !LOOP_BARS_RANGE.contains(&self.loop_bars) {
            return Err(ValidationError::LoopBarsOutOfRange(self.loop_bars));
        }
        for (name, v) in [("fx_reverb", self.fx_reverb), ("fx_chorus", self.fx_chorus)] {
            if let Some(v) = v
                && !(0.0..=1.0).contains(&v)
            {
                return Err(ValidationError::FxOutOfRange { name, value: v });
            }
        }
        if let Some(kit) = self.drum_kit
            && let Some(&bad) = kit.iter().find(|&&n| !(35..=81).contains(&n))
        {
            return Err(ValidationError::DrumNoteOutOfRange { note: bad });
        }
        for track in &self.tracks {
            if track.clips.len() != self.scenes.len() {
                return Err(ValidationError::ClipSlotMismatch {
                    track: track.name.clone(),
                    slots: track.clips.len(),
                    scenes: self.scenes.len(),
                });
            }
            if track.instrument.gm_program > 127 {
                return Err(ValidationError::ProgramOutOfRange(
                    track.instrument.gm_program,
                ));
            }
            if let Some(ch) = track.channel
                && ch > 15
            {
                return Err(ValidationError::ChannelOutOfRange {
                    track: track.name.clone(),
                    channel: ch,
                });
            }
            if let Some(g) = track.gain
                && !(0.0..=2.0).contains(&g)
            {
                return Err(ValidationError::GainOutOfRange {
                    track: track.name.clone(),
                    gain: g,
                });
            }
            if let Some(n) = track.root_note
                && n > 127
            {
                return Err(ValidationError::PitchOutOfRange(n));
            }
            for s in [track.reverb, track.chorus] {
                if let Some(v) = s
                    && !(0.0..=1.0).contains(&v)
                {
                    return Err(ValidationError::SendOutOfRange {
                        track: track.name.clone(),
                        send: v,
                    });
                }
            }
            if let Some(p) = track.pan
                && !(-1.0..=1.0).contains(&p)
            {
                return Err(ValidationError::PanOutOfRange {
                    track: track.name.clone(),
                    pan: p,
                });
            }
            for clip in track.clips.iter().flatten() {
                validate_pattern(&clip.pattern)?;
            }
        }
        for take in &self.takes {
            if !take.is_sorted() {
                return Err(ValidationError::UnsortedTake {
                    take: take.name.clone(),
                });
            }
            for ev in &take.events {
                // Strict: hand-edited project files must not smuggle bad pitches.
                if let RecordedEvent::NoteOff { pitch, .. } | RecordedEvent::NoteOn { pitch, .. } =
                    ev
                    && *pitch > 127
                {
                    return Err(ValidationError::PitchOutOfRange(*pitch));
                }
            }
        }
        Ok(())
    }
}

fn validate_pattern(pattern: &Pattern) -> Result<(), ValidationError> {
    if pattern.bars == 0 || pattern.bars > MAX_PATTERN_BARS {
        return Err(ValidationError::PatternBarsOutOfRange { bars: pattern.bars });
    }
    let expected = pattern.bars as usize * STEPS_PER_BAR;
    if pattern.steps.len() != expected {
        return Err(ValidationError::PatternStepCount(
            pattern.steps.len(),
            expected,
            pattern.bars,
        ));
    }
    for note in pattern.steps.iter().flatten() {
        if note.pitch > 127 {
            return Err(ValidationError::PitchOutOfRange(note.pitch));
        }
        if !(1..=127).contains(&note.velocity) {
            return Err(ValidationError::VelocityOutOfRange(note.velocity));
        }
    }
    Ok(())
}

// -------------------------------------------------------------- matrix rows

/// A track = one FluidSynth internal MIDI channel (D3: track N → channel N).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub name: String,
    /// RGB color for UI/pad feedback, e.g. `[255, 128, 0]`.
    #[serde(default = "default_color")]
    pub color: [u8; 3],
    pub instrument: InstrumentSlot,
    /// One slot per scene (column); `None` = empty cell ("don't change" on
    /// scene launch — R2).
    pub clips: Vec<Option<Clip>>,
    /// Explicit internal-channel routing; `None` = track index (D3). Needed
    /// for a GM drum track, which must sit on channel 9.
    #[serde(default)]
    pub channel: Option<u8>,
    /// Mixer gain multiplier; `None` = 1.0. Applied as CC7 on the track's
    /// channel (mute is CC7=0 — no extra engine state).
    #[serde(default)]
    pub gain: Option<f64>,
    /// Stereo pan −1 (left) ..= +1 (right); `None` = center. Applied as CC10
    /// (`(pan + 1) × 63.5`).
    #[serde(default)]
    pub pan: Option<f64>,
    /// Mute this track (CC7=0 regardless of `gain`).
    #[serde(default)]
    pub mute: bool,
    /// Reverb send 0..=1; `None` = soundfont default. Applied as CC91
    /// (`round(v × 127)`) on the track's channel.
    #[serde(default)]
    pub reverb: Option<f64>,
    /// Chorus send 0..=1; `None` = soundfont default. Applied as CC93.
    #[serde(default)]
    pub chorus: Option<f64>,
    /// Default pitch for SEQ-overlay step editing (D23): used when the
    /// edited clip has no existing pitch to copy. `None` = engine fallback.
    #[serde(default)]
    pub root_note: Option<u8>,
}

fn default_color() -> [u8; 3] {
    [200, 200, 200]
}

/// Instrument binding for a track (R8 patch layer, simplified to its core).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentSlot {
    /// Soundfont file name. Resolved against the box's `sounds/` directory;
    /// `None` = engine default soundfont.
    #[serde(default)]
    pub soundfont: Option<String>,
    /// General MIDI program number 0..=127 (drums: track on channel 9 uses the
    /// GM kit and ignores this — scratchpad D3).
    #[serde(default)]
    pub gm_program: u8,
}

/// A scene = one column of the clip matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    /// Per-scene launch quantize override (D5); `None` = project default.
    #[serde(default)]
    pub quantize: Option<Quantize>,
}

/// When a scene-launch request actually starts playing (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantize {
    Immediate,
    NextBeat,
    /// Default per D5.
    NextBar,
}

/// A clip = one pattern on one track within one scene (a matrix cell).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub name: String,
    pub pattern: Pattern,
}

/// Step sequence at fixed 16th-note resolution (R2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    /// Loop length in bars, 1..=[MAX_PATTERN_BARS]. `steps.len()` must equal
    /// `bars * STEPS_PER_BAR` (enforced by [validate]).
    pub bars: u8,
    pub steps: Vec<Option<StepNote>>,
}

/// A note occupying one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepNote {
    /// MIDI pitch 0..=127.
    pub pitch: u8,
    /// 1..=127.
    pub velocity: u8,
}

// --------------------------------------------------------------- take layer

/// A recorded MIDI performance (D4). Non-destructive; audio is an offline
/// bounce of these events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Take {
    pub name: String,
    /// Events sorted by `tick` (ticks = 16th notes from take start; the fixed
    /// grid keeps storage tiny and needs no tempo math on playback — project
    /// bpm is fixed in v0.1).
    pub events: Vec<RecordedEvent>,
    /// Silent during take playback (kept, not deleted — additive field, v1-safe).
    #[serde(default)]
    pub muted: bool,
}

impl Take {
    /// Playback loop length in ticks for this take given the project floor:
    /// max(loop_bars, whole bars covered by the take). Never truncates — a
    /// long take loops at its own (rounded-up) length.
    pub fn loop_ticks(&self, loop_bars: u8) -> u32 {
        let last = self
            .events
            .iter()
            .map(|e| match e {
                RecordedEvent::NoteOn { tick, .. }
                | RecordedEvent::NoteOff { tick, .. }
                | RecordedEvent::ProgramChange { tick, .. } => *tick,
            })
            .max()
            .unwrap_or(0);
        let bars = (last.div_ceil(STEPS_PER_BAR as u32)).max(1);
        (bars.max(loop_bars as u32) * STEPS_PER_BAR as u32).max(1)
    }

    /// Events exactly at `tick` (take-local coordinates). Relies on the
    /// sorted-by-tick invariant (enforced by [`Project::validate`]).
    pub fn events_at(&self, tick: u32) -> &[RecordedEvent] {
        let start = self.events.partition_point(|e| e.tick() < tick);
        let end = self.events.partition_point(|e| e.tick() <= tick);
        &self.events[start..end]
    }

    /// Sorted-by-tick invariant check.
    pub fn is_sorted(&self) -> bool {
        self.events.windows(2).all(|w| w[0].tick() <= w[1].tick())
    }
}

impl RecordedEvent {
    /// The tick position, shared accessor across variants.
    pub fn tick(&self) -> u32 {
        match self {
            RecordedEvent::NoteOn { tick, .. }
            | RecordedEvent::NoteOff { tick, .. }
            | RecordedEvent::ProgramChange { tick, .. } => *tick,
        }
    }
}

/// Minimal event set for takes in v0.1 (MIDI-only recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedEvent {
    NoteOn {
        tick: u32,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    NoteOff {
        tick: u32,
        channel: u8,
        pitch: u8,
    },
    ProgramChange {
        tick: u32,
        channel: u8,
        program: u8,
    },
}

#[cfg(test)]
mod tests;
