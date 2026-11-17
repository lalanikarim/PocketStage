//! Configurable DAW-mode keymap (D26 / D26.1, schema **v0.1**).
//!
//! The DAW-mode *semantic* layer — voice browser, overlay commands, fader
//! buttons — as data. Full plan: `specs/daw-keymap.md`; rulings: scratchpad
//! R14 + addendum. This module is pure (serde + validation only): loading,
//! persisting and hot-reloading `~/.pocketstage/keymap.json` is the server
//! crate's job (phase 2); consuming it is the engine feeder's job (phase 3).
//!
//! Hard invariants carried from the spec:
//! - One frozen **source** vocabulary everywhere: `{"note":N}` / `{"cc":N}`
//!   plus optional `"port":"music"|"hui"` (absent = any). Notes and CCs are
//!   disjoint number spaces — note 44 and cc 44 may coexist.
//! - PRESET instrument routing is **not expressible** in this schema at all
//!   ("can't remap preset" by construction).
//! - `Default` reproduces the engine's hardcoded table exactly (regression
//!   lock; see `defaults_match_engine_hardcoded_table`).
//! - `knobs` ships empty, but its target semantics are now ruled (D36):
//!   each row carries a [`KnobTarget`] (relative/absolute value mode + a
//!   closed `param` leaf). The frozen SOURCE object is untouched — that is
//!   exactly what freezing it bought us.

use pocketstage_model::{ActiveVoice, MAX_TRACKS};
use serde::{Deserialize, Serialize};

/// Internal schema stamp (Q-V3: no version *field* until the first real
/// incompatible change; this constant is what phase-2 code logs/gates on).
pub const KEYMAP_SCHEMA: u8 = 1;

/// Display-label length cap (chars) for voice bindings.
pub const LABEL_MAX: usize = 32;

/// Which physical port a source arrives on. `port` qualifiers exist because
/// measured ground truth routes DAW-mode controls on the HUI port and
/// keys/pads on the music port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Port {
    Music,
    Hui,
}

/// Number space of a source: notes and CCs never collide with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    Note,
    Cc,
}

impl Space {
    fn name(self) -> &'static str {
        match self {
            Space::Note => "note",
            Space::Cc => "cc",
        }
    }
}

/// Frozen v0.1 binding source: a note or a CC, optionally qualified by port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Note { note: u8, port: Option<Port> },
    Cc { cc: u8, port: Option<Port> },
}

impl Source {
    pub fn note(n: u8) -> Self {
        Self::Note {
            note: n,
            port: None,
        }
    }

    pub fn note_on(n: u8, port: Port) -> Self {
        Self::Note {
            note: n,
            port: Some(port),
        }
    }

    pub fn cc(c: u8) -> Self {
        Self::Cc { cc: c, port: None }
    }

    pub fn cc_on(c: u8, port: Port) -> Self {
        Self::Cc {
            cc: c,
            port: Some(port),
        }
    }

    /// The number in this source's own space (note or CC value).
    pub fn number(&self) -> u8 {
        match self {
            Source::Note { note, .. } | Source::Cc { cc: note, .. } => *note,
        }
    }

    pub fn space(&self) -> Space {
        match self {
            Source::Note { .. } => Space::Note,
            Source::Cc { .. } => Space::Cc,
        }
    }

    pub fn port(&self) -> Option<Port> {
        match self {
            Source::Note { port, .. } | Source::Cc { port, .. } => *port,
        }
    }

    /// Does a binding on `self` accept an event arriving on `incoming`?
    /// A binding without a port qualifier accepts any port.
    pub fn matches_port(&self, incoming: Port) -> bool {
        match self.port() {
            None => true,
            Some(p) => p == incoming,
        }
    }
}

/// Two sources collide when they share space + number and their port
/// qualifiers overlap — an unqualified binding overlaps every qualifier.
fn ports_conflict(a: Option<Port>, b: Option<Port>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(x), Some(y)) => x == y,
    }
}

// --- serde shape for Source: {"note":N,"port"?} | {"cc":N,"port"?} --------

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceRepr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cc: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<Port>,
}

impl TryFrom<SourceRepr> for Source {
    type Error = String;

    fn try_from(r: SourceRepr) -> Result<Self, Self::Error> {
        match (r.note, r.cc) {
            (Some(note), None) => Ok(Source::Note { note, port: r.port }),
            (None, Some(cc)) => Ok(Source::Cc { cc, port: r.port }),
            (Some(_), Some(_)) => Err("source has both `note` and `cc`".into()),
            (None, None) => Err("source needs exactly one of `note` or `cc`".into()),
        }
    }
}

impl From<Source> for SourceRepr {
    fn from(s: Source) -> Self {
        match s {
            Source::Note { note, port } => Self {
                note: Some(note),
                cc: None,
                port,
            },
            Source::Cc { cc, port } => Self {
                note: None,
                cc: Some(cc),
                port,
            },
        }
    }
}

impl Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SourceRepr::from(*self).serialize(s)
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(SourceRepr::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

// --- commands --------------------------------------------------------------

/// Momentary semantic commands. Vocabulary FROZEN at v0.1. `bank_prev`/
/// `bank_next` = MIX-overlay paging mirroring the documented `<`/`>`
/// track-bank scroll, and they **ship unbound permanently** (D30): T7
/// measured their numbers (HUI notes 46/47) but they are DAW-mode-only —
/// silent in PRESET, which is where MIX lives. No default, and note that a
/// hand-written binding has **no engine consumer** today: the feeder matches
/// these two to an empty arm (no paging `Event` exists), so they are vocabulary
/// kept for wire-format stability only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    MixToggle,
    SeqToggle,
    SeqNextBar,
    SeqFlip,
    BankPrev,
    BankNext,
}

// --- bindings --------------------------------------------------------------

/// source → voice row. `voice` stays an externally-tagged enum (Addition A)
/// so a future `{"patch": ...}` variant is additive without breaking files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceBinding {
    pub source: Source,
    pub voice: ActiveVoice,
    pub label: String,
}

/// source → momentary command row (notes in the default map; any source
/// legal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandBinding {
    pub source: Source,
    pub command: Command,
}

/// Per-fader-button command row (Q2). Sources are **notes** on the HUI port:
/// T7 take 2 measured buttons 1–8 as HUI notes 0–7 (button N → note N−1);
/// the 9th button is DAW BUTTON MODE (device-local, never a target). Ships
/// empty — buttons already work as channel selectors via the built-in mapper
/// path; a row here reassigns one to a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaderButtonBinding {
    pub source: Source,
    pub command: Command,
}

// --- knob targets (D36) ---------------------------------------------------

/// Measured relative-rotor tick constants (T7 take 2, device map): clockwise
/// emits value 1, counter-clockwise emits value 65. One message per tick,
/// never position data. Any other value on a bound CC is ignored.
pub const RELATIVE_CW: u8 = 1;
pub const RELATIVE_CCW: u8 = 65;

/// How a knob's wire values map onto the target param's normalized range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobKind {
    /// Each message is one tick: [`RELATIVE_CW`]/[`RELATIVE_CCW`] move the
    /// param by `step`; other values are dropped (defensive — measured units
    /// only ever emit the two constants).
    Relative,
    /// CC value 0..=127 maps linearly onto the param's normalized 0..=1 range.
    Absolute,
}

/// Per-track mixer field, the same four the MIX overlay rides (D22) — the
/// "MIX becomes data" endgame shares this namespace on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackField {
    Gain,
    Pan,
    Reverb,
    Chorus,
}

/// Project-level params reachable by a knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlobalParam {
    MasterGain,
    FxReverb,
    FxChorus,
}

/// One track field target: 1-based `index` (same convention as fader/knob
/// control indices), field within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackTarget {
    pub index: u8,
    pub field: TrackField,
}

/// The param leaf — **closed vocabulary v1** (D36). JSON:
/// `{"track":{"index":2,"field":"pan"}}` or `{"global":"master_gain"}`.
/// Adding a variant is additive; item-19 patch refs will extend the *voice*
/// field (Addition A), not this leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KnobParam {
    Track(TrackTarget),
    Global(GlobalParam),
}

/// Default relative step: 0.02 normalized units per tick (50 per sweep).
pub fn default_step() -> f64 {
    0.02
}

/// A knob target: value mode + step + param. All params live in a normalized
/// 0..=1 domain; the engine converts to model ranges (pan bipolar, CC math)
/// with the exact same math as the MIX overlay, so a knob ride and a web
/// PUT cannot diverge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct KnobTarget {
    pub kind: KnobKind,
    /// Normalized units per relative tick. Ignored by `absolute`. Range
    /// (0.0..=1.0]; default 0.02 (50 ticks full-scale).
    #[serde(default = "default_step")]
    pub step: f64,
    pub param: KnobParam,
}

impl KnobTarget {
    /// One wire message + the param's current normalized value → next
    /// normalized value (`None` = an ignored relative value). Pure; clamping
    /// to 0..=1 is the caller that owns the domain.
    pub fn apply(&self, value: u8, current: f64) -> Option<f64> {
        match self.kind {
            KnobKind::Absolute => Some((value as f64 / 127.0).clamp(0.0, 1.0)),
            KnobKind::Relative => match value {
                RELATIVE_CW => Some(current + self.step),
                RELATIVE_CCW => Some(current - self.step),
                _ => None,
            },
        }
    }
}

/// Knob row: source (frozen v0.1) + target (D36). Ships empty by default —
/// guessed defaults are how silent misroutes happen, and the built-in MIX
/// knob page stays the behavior for unbound CCs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnobBinding {
    pub source: Source,
    pub target: KnobTarget,
}

// --- the keymap ------------------------------------------------------------

fn default_voices() -> Vec<VoiceBinding> {
    // Mirrors engine main.rs: VOICE_BROWSER_ANCHOR=36, 12 white-key slots
    // C2..G3 with the curated GM patches (D25). Keep in lockstep — the
    // regression test below asserts this exact table.
    //
    // D30: every row is **music-port qualified**, not any-port. The browser is
    // keybed/pad inventory and the measured map puts that on port :0 (the HUI
    // port carries transport + Mackie-style strip controls, never our keys) —
    // see specs/device-maps/oxygen-49-mkv.md. Qualifying it frees the entire
    // HUI note space for DAW-mode command defaults; unqualified rows claimed
    // numbers like 46/47 on every port and made such bindings unverifiable as
    // duplicates.
    let white_keys = [36u8, 38, 40, 41, 43, 45, 47, 48, 50, 52, 53, 55];
    let voices: [(ActiveVoice, &str); 12] = [
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
    white_keys
        .iter()
        .zip(voices)
        .map(|(n, (voice, label))| VoiceBinding {
            source: Source::note_on(*n, Port::Music),
            voice,
            label: label.into(),
        })
        .collect()
}

fn default_commands() -> Vec<CommandBinding> {
    // Engine main.rs: CMD_MIX=56 G#3, CMD_SEQ=58 A#3, CMD_NEXT=61 C#4,
    // CMD_FLIP=63 D#4. Music-port qualified like the voices (D30): these are
    // keybed black keys, and leaving them unqualified would silently claim the
    // same numbers on the HUI port, where they are Mackie strip buttons.
    vec![
        CommandBinding {
            source: Source::note_on(56, Port::Music),
            command: Command::MixToggle,
        },
        CommandBinding {
            source: Source::note_on(58, Port::Music),
            command: Command::SeqToggle,
        },
        CommandBinding {
            source: Source::note_on(61, Port::Music),
            command: Command::SeqNextBar,
        },
        CommandBinding {
            source: Source::note_on(63, Port::Music),
            command: Command::SeqFlip,
        },
    ]
}

/// The frozen v0.1 keymap document. Each section defaults independently, so
/// a JSON file may override just one section; `{}` (or an empty file via
/// [`DawKeymap::from_json_str`]) yields the built-in defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DawKeymap {
    #[serde(default = "default_voices")]
    pub voices: Vec<VoiceBinding>,
    #[serde(default = "default_commands")]
    pub commands: Vec<CommandBinding>,
    #[serde(default)]
    pub fader_buttons: Vec<FaderButtonBinding>,
    #[serde(default)]
    pub knobs: Vec<KnobBinding>,
}

impl Default for DawKeymap {
    fn default() -> Self {
        Self {
            voices: default_voices(),
            commands: default_commands(),
            fader_buttons: Vec::new(),
            knobs: Vec::new(),
        }
    }
}

impl DawKeymap {
    /// Human-readable validation (`Ok(())` or first error), same discipline
    /// as [`crate::Mapping`].
    pub fn validate(&self) -> Result<(), String> {
        for (i, b) in self.voices.iter().enumerate() {
            check_source(&b.source, &format!("voices[{i}]"))?;
            if let ActiveVoice::Program(p) = b.voice
                && p > 127
            {
                return Err(format!("voices[{i}] program {p} out of range 0..=127"));
            }
            let len = b.label.chars().count();
            if len == 0 {
                return Err(format!("voices[{i}] label is empty"));
            }
            if len > LABEL_MAX {
                return Err(format!(
                    "voices[{i}] label {len} chars exceeds cap {LABEL_MAX}"
                ));
            }
        }
        for (i, b) in self.commands.iter().enumerate() {
            check_source(&b.source, &format!("commands[{i}]"))?;
        }
        for (i, b) in self.fader_buttons.iter().enumerate() {
            check_source(&b.source, &format!("fader_buttons[{i}]"))?;
        }
        for (i, b) in self.knobs.iter().enumerate() {
            let what = format!("knobs[{i}]");
            check_source(&b.source, &what)?;
            check_knob_target(&b.target, &what)?;
        }
        let dups = self.duplicates();
        if !dups.is_empty() {
            return Err(format!("duplicate sources: {}", dups.join("; ")));
        }
        Ok(())
    }

    /// All cross-section source collisions, one message per colliding pair
    /// (per number space, port-aware). Exposed for the UI as well as
    /// [`Self::validate`].
    pub fn duplicates(&self) -> Vec<String> {
        let mut entries: Vec<(Space, u8, Option<Port>, String)> = Vec::new();
        let mut push = |src: &Source, what: String| {
            entries.push((src.space(), src.number(), src.port(), what));
        };
        for (i, b) in self.voices.iter().enumerate() {
            push(&b.source, format!("voices[{i}] ({})", b.label));
        }
        for (i, b) in self.commands.iter().enumerate() {
            push(&b.source, format!("commands[{i}]"));
        }
        for (i, b) in self.fader_buttons.iter().enumerate() {
            push(&b.source, format!("fader_buttons[{i}]"));
        }
        for (i, b) in self.knobs.iter().enumerate() {
            push(&b.source, format!("knobs[{i}]"));
        }
        let mut out = Vec::new();
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                let (s1, n1, p1, d1) = &entries[i];
                let (s2, n2, p2, d2) = &entries[j];
                if s1 == s2 && n1 == n2 && ports_conflict(*p1, *p2) {
                    out.push(format!(
                        "{d1} and {d2} both claim {} {n1}{port}",
                        s1.name(),
                        port = p_suffix(*p1),
                    ));
                }
            }
        }
        out
    }

    /// Voice bound to a note arriving on `incoming` (None = unbound).
    pub fn voice_for(&self, note: u8, incoming: Port) -> Option<&VoiceBinding> {
        self.voices.iter().find(|b| {
            matches!(b.source, Source::Note { note: n, .. } if n == note)
                && b.source.matches_port(incoming)
        })
    }

    /// Knob row claiming a CC arriving on `incoming` (None = unbound).
    /// Matching is raw-CC level — space + number + port qualifier, same
    /// collision rules as every other section (D36: the feeder consumes
    /// these before the built-in MIX knob page gets them).
    pub fn knob_for(&self, source: Source, incoming: Port) -> Option<&KnobBinding> {
        self.knobs.iter().find(|b| {
            b.source.space() == source.space()
                && b.source.number() == source.number()
                && b.source.matches_port(incoming)
        })
    }

    /// Command bound to a source arriving on `incoming`, checking `commands`
    /// first, then `fader_buttons` (None = unbound). Validation guarantees
    /// no cross-section collisions, so lookup order never matters in
    /// practice; commands win defensively.
    pub fn command_for(&self, source: Source, incoming: Port) -> Option<Command> {
        let hit = |src: &Source| {
            src.space() == source.space()
                && src.number() == source.number()
                && src.matches_port(incoming)
        };
        if let Some(b) = self.commands.iter().find(|b| hit(&b.source)) {
            return Some(b.command);
        }
        self.fader_buttons
            .iter()
            .find(|b| hit(&b.source))
            .map(|b| b.command)
    }

    /// Parse a keymap file. A blank/whitespace-only file yields defaults
    /// (the "corrupt or missing → defaults, never fail boot" rule is the
    /// loader's; empty is *defined* as default here).
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(s)
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).expect("keymap always serializes")
    }
}

fn p_suffix(p: Option<Port>) -> String {
    match p {
        None => String::new(),
        Some(Port::Music) => " (port music)".into(),
        Some(Port::Hui) => " (port hui)".into(),
    }
}

fn check_knob_target(t: &KnobTarget, what: &str) -> Result<(), String> {
    if let KnobParam::Track(tt) = t.param
        && !(1..=MAX_TRACKS as u8).contains(&tt.index)
    {
        return Err(format!(
            "{what} track index {} out of range 1..={MAX_TRACKS}",
            tt.index
        ));
    }
    // Step is validated for both kinds: an absolute row with a nonsense step
    // parses today and would surprise whoever flips it to relative tomorrow.
    if !t.step.is_finite() || t.step <= 0.0 || t.step > 1.0 {
        return Err(format!(
            "{what} step {} out of range (0 exclusive ..= 1.0)",
            t.step
        ));
    }
    Ok(())
}

fn check_source(src: &Source, what: &str) -> Result<(), String> {
    let n = src.number();
    if n > 127 {
        return Err(format!(
            "{what} {} {n} out of range 0..=127",
            src.space().name()
        ));
    }
    Ok(())
}
