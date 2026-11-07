//! The mode-aware event mapper: decoded MIDI events → semantic actions.
//!
//! Pure logic — no I/O, no allocation on the hot path beyond what `Event`
//! already carries. Feed it everything from the pump's `Midi(MidiInputMessage)`
//! stream; it decides what is a control and what to ignore (TimingClock spam,
//! aftertouch, program changes...).
//!
//! Scope of part 1 (input mapping). LED feedback requires the T2 SysEx
//! protocol and will live beside this module once known.

use crate::mapping::Mapping;
use pocketstage_midi::Event;

/// A semantic action performed on the Oxygen 49 MKV (preset mode).
///
/// Deliberately device-level: *what physical thing happened*, not what it
/// means for the song. Deciding that a pad launches a clip is engine policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Piano key (outside the pad note range).
    Key { note: u8, on: bool, velocity: u8 },
    /// Velocity pad, `index` is 1-based.
    Pad { index: u8, on: bool, velocity: u8 },
    /// Motor fader position, `index` is 1-based.
    Fader { index: u8, value: u8 },
    /// Button under a fader, `index` is 1-based.
    FaderButton { index: u8, pressed: bool },
    /// Rotary knob, `index` is 1-based (physical order per measured map).
    Knob { index: u8, value: u8 },
    /// Mod wheel (keys channel).
    ModWheel(u8),
    /// Pitch lever, centered at 0 (−8192..=8191).
    PitchBend(i16),
    /// System Start (`FA`) — observed from the ▶ button in preset mode.
    Play,
    /// System Stop (`FC`) — observed from the ⏹ button.
    Stop,
    /// System Reset (`FF`) — observed around transport presses; treat with
    /// care (probe A2 will pin down which buttons emit it).
    SystemReset,
}

/// Maps decoded events to [`Action`]s through a [`Mapping`].
#[derive(Debug, Clone)]
pub struct Mapper {
    m: Mapping,
}

impl Mapper {
    pub fn new(m: Mapping) -> Self {
        Self { m }
    }

    pub fn mapping(&self) -> &Mapping {
        &self.m
    }

    /// Event channel is 0-based; mapping channels are 1-based.
    fn on_channel(ev_ch0: u8, map_ch1: u8) -> bool {
        ev_ch0.saturating_add(1) == map_ch1
    }

    pub fn map(&self, ev: &Event) -> Option<Action> {
        match ev {
            Event::NoteOn {
                channel,
                key,
                velocity,
            } => Some(self.note(*channel, *key, true, *velocity)),
            Event::NoteOff { channel, key, .. } => Some(self.note(*channel, *key, false, 0)),

            Event::ControlChange {
                channel,
                control,
                value,
            } => self.cc(*channel, *control, *value),

            Event::PitchBend { channel, value }
                if Self::on_channel(*channel, self.m.keys_channel) =>
            {
                Some(Action::PitchBend(*value as i16 - 8192))
            }

            Event::Start => Some(Action::Play),
            Event::Stop => Some(Action::Stop),
            Event::Reset => Some(Action::SystemReset),

            // Everything else is intentionally silent: TimingClock (the unit
            // streams ~45/s), ActiveSensing, Continue, SysEx, program change,
            // aftertouch. Unknown stays invisible until probes explain it.
            _ => None,
        }
    }

    fn note(&self, channel: u8, key: u8, on: bool, velocity: u8) -> Action {
        if Self::on_channel(channel, self.m.keys_channel)
            && key >= self.m.pad_note_base
            && key < self.m.pad_note_base + self.m.pad_count
        {
            Action::Pad {
                index: key - self.m.pad_note_base + 1,
                on,
                velocity,
            }
        } else {
            Action::Key {
                note: key,
                on,
                velocity,
            }
        }
    }

    fn cc(&self, channel: u8, control: u8, value: u8) -> Option<Action> {
        let controls_here = if self.m.control_any_channel {
            true
        } else {
            Self::on_channel(channel, self.m.control_channel)
        };
        if controls_here {
            if let Some(pos) = self.m.knobs.iter().position(|&c| c == control) {
                return Some(Action::Knob {
                    index: pos as u8 + 1,
                    value,
                });
            }
            if control >= self.m.fader_base_cc
                && control < self.m.fader_base_cc + self.m.fader_count
            {
                return Some(Action::Fader {
                    index: control - self.m.fader_base_cc + 1,
                    value,
                });
            }
            if control >= self.m.fader_button_base_cc
                && control < self.m.fader_button_base_cc + self.m.fader_button_count
            {
                return Some(Action::FaderButton {
                    index: control - self.m.fader_button_base_cc + 1,
                    pressed: value >= 64,
                });
            }
        }
        if Self::on_channel(channel, self.m.keys_channel) && control == self.m.mod_wheel_cc {
            return Some(Action::ModWheel(value));
        }
        None
    }
}
