//! Reassignable control map for the Oxygen 49 MKV (preset mode).
//!
//! Defaults encode the *measured* factory behavior from
//! `specs/device-maps/oxygen-49-mkv.md` (T1 capture, July 2026). Users may
//! override any field via a JSON file; serde rejects unknown fields so typos
//! fail loudly instead of silently doing nothing.

use serde::{Deserialize, Serialize};

/// Which MIDI messages correspond to which physical controls.
///
/// All channels are **1-based** (MIDI manual style); the mapper converts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    /// Channel the knobs/faders/fader-buttons transmit on (unit-side setting;
    /// factory default in our capture was 13).
    pub control_channel: u8,
    /// Ignore `control_channel` and accept control CCs from any channel.
    /// Useful while the unit's channel setting is in flux.
    #[serde(default)]
    pub control_any_channel: bool,
    /// Channel of the keys, pads, mod wheel and pitch lever.
    pub keys_channel: u8,
    /// First pad note (logical pad 1). Measured: 36; pads ascend chromatically.
    /// With **16** logical pads the two hardware banks map to:
    /// notes 36–43 = SHIFT+pad1 bank, 44–51 = SHIFT+pad2 bank.
    /// (Caveat: notes 44–51 are also low piano keys — shadowed by design.)
    pub pad_note_base: u8,
    pub pad_count: u8,
    /// Mod-wheel CC on the keys channel.
    pub mod_wheel_cc: u8,
    /// Knob CCs in physical order (knob 1 first). Measured sequence is
    /// deliberately non-contiguous — do not "fix" it.
    pub knobs: Vec<u8>,
    /// First fader CC (fader 1); faders ascend.
    pub fader_base_cc: u8,
    pub fader_count: u8,
    /// First fader-button CC; measured only 8 for 9 physical buttons:
    /// **button #9 collides with fader 9 — it also sends CC41** (probe A1).
    /// Keep this count at 8 unless you enjoy ambiguity.
    pub fader_button_base_cc: u8,
    pub fader_button_count: u8,
    /// Fader indices whose BUTTON shares the fader's position CC (probe B:
    /// factory collision on #9 — the button sends bare 127/0 pairs on CC41).
    /// The mapper detects `127→0` with no intermediate values and reports
    /// [`Action::FaderButtonTap`] instead of corrupting the fader position.
    #[serde(default = "shared_button_faders_default")]
    pub shared_button_faders: Vec<u8>,
    /// Transport buttons mirrored as notes on the Mackie/HUI port (port :1).
    /// Enabled only for events arriving from that port — these note numbers
    /// are ordinary top-octave keys on the music port! `None` by default.
    #[serde(default)]
    pub transport_notes: Option<TransportNotes>,
    /// CC of the **DAW button** pulse on the HUI port (T6: keys channel,
    /// momentary 127→0). Only consulted when `transport_notes` is set.
    #[serde(default = "mode_daw_cc_default")]
    pub mode_daw_cc: u8,
    /// CC of the **PRESET button** pulse on the HUI port (T6). Only consulted
    /// when `transport_notes` is set.
    #[serde(default = "mode_preset_cc_default")]
    pub mode_preset_cc: u8,
}

fn mode_daw_cc_default() -> u8 {
    113
}

fn mode_preset_cc_default() -> u8 {
    112
}

fn shared_button_faders_default() -> Vec<u8> {
    vec![9]
}

/// Note numbers the transport buttons emit on the HUI port (probe A2):
/// loop 86, stop 93, play 94, record 95 — all ch1, vel 127.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportNotes {
    pub loop_note: u8,
    pub stop_note: u8,
    pub play_note: u8,
    pub record_note: u8,
}

impl Default for TransportNotes {
    fn default() -> Self {
        Self {
            loop_note: 86,
            stop_note: 93,
            play_note: 94,
            record_note: 95,
        }
    }
}

/// Factory-default map as measured on the real unit (see module docs).
pub fn default_mapping() -> Mapping {
    Mapping {
        control_channel: 13,
        control_any_channel: false,
        keys_channel: 1,
        pad_note_base: 36,
        pad_count: 16,
        mod_wheel_cc: 1,
        knobs: vec![44, 45, 46, 47, 62, 63, 75, 76],
        fader_base_cc: 33,
        fader_count: 9,
        fader_button_base_cc: 49,
        fader_button_count: 8,
        shared_button_faders: vec![9],
        transport_notes: None,
        mode_daw_cc: mode_daw_cc_default(),
        mode_preset_cc: mode_preset_cc_default(),
    }
}

/// Mapping for events arriving on the **HUI port** (:1): transport notes and
/// the DAW/PRESET mode pulses (T6) are live there; nothing else from that
/// port should ever map (it also carries Mackie broadcasts we ignore).
pub fn hui_mapping() -> Mapping {
    Mapping {
        transport_notes: Some(TransportNotes::default()),
        ..default_mapping()
    }
}

impl Mapping {
    /// Validate internal consistency. Returns `Ok(())` or a human message.
    pub fn validate(&self) -> Result<(), String> {
        let one_to_16 = |n: u8, what: &str| {
            if (1..=16).contains(&n) {
                Ok(())
            } else {
                Err(format!("{what} must be 1..=16, got {n}"))
            }
        };
        one_to_16(self.control_channel, "control_channel")?;
        one_to_16(self.keys_channel, "keys_channel")?;
        if self.pad_count == 0 || self.pad_note_base as u16 + self.pad_count as u16 > 128 {
            return Err(format!(
                "pad range {}..{} out of bounds",
                self.pad_note_base,
                self.pad_note_base + self.pad_count
            ));
        }
        if self.knobs.is_empty() || self.knobs.iter().any(|&c| c == 0 || c > 119) {
            return Err("knobs must be non-empty, CCs 1..=119".into());
        }
        let cc_range_ok = |base: u8, count: u8, what: &str| {
            if base as u16 + count as u16 > 120 || count == 0 {
                Err(format!(
                    "{what} CC{base}+{count} outside usable range 1..=119"
                ))
            } else {
                Ok(())
            }
        };
        cc_range_ok(self.fader_base_cc, self.fader_count, "faders")?;
        cc_range_ok(
            self.fader_button_base_cc,
            self.fader_button_count,
            "fader buttons",
        )?;
        // Duplicate CCs across groups make mapping ambiguous.
        let mut seen = std::collections::HashSet::new();
        let mut claim = |cc: u8, what: &str| -> Result<(), String> {
            if seen.insert(cc) {
                Ok(())
            } else {
                Err(format!("CC{cc} claimed twice (at {what})"))
            }
        };
        for (i, &cc) in self.knobs.iter().enumerate() {
            claim(cc, &format!("knob {}", i + 1))?;
        }
        for i in 0..self.fader_count {
            claim(self.fader_base_cc + i, &format!("fader {}", i + 1))?;
        }
        for i in 0..self.fader_button_count {
            claim(
                self.fader_button_base_cc + i,
                &format!("fader button {}", i + 1),
            )?;
        }
        if self.mode_daw_cc == 0 || self.mode_daw_cc > 119 {
            return Err("mode_daw_cc must be 1..=119".into());
        }
        if self.mode_preset_cc == 0 || self.mode_preset_cc > 119 {
            return Err("mode_preset_cc must be 1..=119".into());
        }
        if let Some(tn) = &self.transport_notes {
            for (note, what) in [
                (tn.loop_note, "loop"),
                (tn.stop_note, "stop"),
                (tn.play_note, "play"),
                (tn.record_note, "record"),
            ] {
                if !(24..=108).contains(&note) {
                    return Err(format!("transport {what} note {note} out of range"));
                }
            }
        }
        for &idx in &self.shared_button_faders {
            if idx == 0 || idx > self.fader_count {
                return Err(format!("shared-button fader index {idx} out of range"));
            }
        }
        Ok(())
    }

    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json_string(&self) -> String {
        // Pretty; intended for writing a starting-point file for users.
        serde_json::to_string_pretty(self).expect("mapping always serializes")
    }
}
