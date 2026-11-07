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
    /// First pad note (pad 1). Measured: 36 (C1), pads ascend chromatically.
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
    /// First fader-button CC; measured only 8 for 9 physical buttons —
    /// button #9 is pending probe A1, so the default count stays 8.
    pub fader_button_base_cc: u8,
    pub fader_button_count: u8,
}

/// Factory-default map as measured on the real unit (see module docs).
pub fn default_mapping() -> Mapping {
    Mapping {
        control_channel: 13,
        control_any_channel: false,
        keys_channel: 1,
        pad_note_base: 36,
        pad_count: 8,
        mod_wheel_cc: 1,
        knobs: vec![44, 45, 46, 47, 62, 63, 75, 76],
        fader_base_cc: 33,
        fader_count: 9,
        fader_button_base_cc: 49,
        fader_button_count: 8,
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
