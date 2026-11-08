//! The mode-aware event mapper: decoded MIDI events → semantic actions.
//!
//! Pure logic — no I/O. Feed it everything from the pump's `Midi(MidiInputMessage)`
//! stream (per originating port; see [`crate::hui_mapping`]); it decides what is
//! a control and what to ignore (TimingClock spam, aftertouch, program changes...).
//!
//! Stateful for one reason: fader 9's button collides with its position CC
//! (probe B). A bare `127 → 0` pair on that CC — no intermediate values — is a
//! button **tap**; the two corrupting position events are swallowed. Any other
//! event flushes the held value as normal fader data.

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
    /// Velocity pad, `index` 1-based over the configured window; with the
    /// default 16-pad window, 9–16 are the SHIFT+pad2 bank.
    Pad { index: u8, on: bool, velocity: u8 },
    /// Motor fader position, `index` is 1-based.
    Fader { index: u8, value: u8 },
    /// Button under a fader (own CC), `index` is 1-based.
    FaderButton { index: u8, pressed: bool },
    /// Tap of a button that shares its fader's CC (fader 9, probe B).
    /// Emitted once per press; the colliding position events are suppressed.
    FaderButtonTap { index: u8 },
    /// Rotary knob, `index` is 1-based (physical order per measured map).
    Knob { index: u8, value: u8 },
    /// Mod wheel (keys channel).
    ModWheel(u8),
    /// Pitch lever, centered at 0 (−8192..=8191).
    PitchBend(i16),
    /// ▶ — from HUI note 94 (or system Start on the music port).
    Play,
    /// ⏹ — from HUI note 93 (or system Stop).
    Stop,
    /// 🔁 — **only** observable via HUI note 86; emits nothing else (probe A2).
    LoopToggle,
    /// ⏺ — **only** observable via HUI note 95; on the music port it is
    /// indistinguishable from Play (both send Start + Reset·Reset).
    Record,
    /// System Reset (`FF`) — observed around transport presses.
    SystemReset,
}

/// Maps decoded events to [`Action`]s through a [`Mapping`]. Stateful only for
/// the shared-button detector; call sites own one `Mapper` per source port.
#[derive(Debug)]
pub struct Mapper {
    m: Mapping,
    /// (fader index, held 127 value) while a possible button tap is unresolved.
    pending_tap: Option<u8>,
}

impl Mapper {
    pub fn new(m: Mapping) -> Self {
        Self {
            m,
            pending_tap: None,
        }
    }

    pub fn mapping(&self) -> &Mapping {
        &self.m
    }

    /// Event channel is 0-based; mapping channels are 1-based.
    fn on_channel(ev_ch0: u8, map_ch1: u8) -> bool {
        ev_ch0.saturating_add(1) == map_ch1
    }

    /// Decode one event; actions are appended to `out` (callers reuse the
    /// Vec; at most two actions are produced by any single event).
    pub fn map(&mut self, ev: &Event, out: &mut Vec<Action>) {
        // Anything that is not the shared fader's own CC ends a possible tap
        // sequence — flush the held top position so mixer state stays true.
        let shared_cc = matches!(ev,
            Event::ControlChange { channel, control, .. }
                if self.shared_button_fader_for(*channel, *control).is_some());
        if !shared_cc && let Some(idx) = self.pending_tap.take() {
            out.push(Action::Fader {
                index: idx,
                value: 127,
            });
        }
        match ev {
            Event::NoteOn {
                channel,
                key,
                velocity,
            } => self.note(*channel, *key, true, *velocity, out),
            Event::NoteOff { channel, key, .. } => self.note(*channel, *key, false, 0, out),

            Event::ControlChange {
                channel,
                control,
                value,
            } => self.cc(*channel, *control, *value, out),

            Event::PitchBend { channel, value }
                if Self::on_channel(*channel, self.m.keys_channel) =>
            {
                out.push(Action::PitchBend(*value as i16 - 8192));
            }

            Event::Start => out.push(Action::Play),
            Event::Stop => out.push(Action::Stop),
            Event::Reset => out.push(Action::SystemReset),

            // Everything else is intentionally silent: TimingClock (the unit
            // streams ~45/s), ActiveSensing, Continue, SysEx, program change,
            // aftertouch. Unknown stays invisible until probes explain it.
            _ => {}
        }
    }

    fn note(&mut self, channel: u8, key: u8, on: bool, velocity: u8, out: &mut Vec<Action>) {
        if Self::on_channel(channel, self.m.keys_channel) {
            // Transport mirror (HUI port only — mapping has it enabled there).
            if let Some(tn) = &self.m.transport_notes {
                if on {
                    if key == tn.loop_note {
                        out.push(Action::LoopToggle);
                    } else if key == tn.stop_note {
                        out.push(Action::Stop);
                    } else if key == tn.play_note {
                        out.push(Action::Play);
                    } else if key == tn.record_note {
                        out.push(Action::Record);
                    }
                }
                // On the HUI port, notes are *only* transport; never keys.
                return;
            }
            if key >= self.m.pad_note_base && key < self.m.pad_note_base + self.m.pad_count {
                out.push(Action::Pad {
                    index: key - self.m.pad_note_base + 1,
                    on,
                    velocity,
                });
                return;
            }
        }
        out.push(Action::Key {
            note: key,
            on,
            velocity,
        });
    }

    fn cc(&mut self, channel: u8, control: u8, value: u8, out: &mut Vec<Action>) {
        // Shared-button fader? Its position CC needs the tap detector.
        if let Some(idx) = self.shared_button_fader_for(channel, control) {
            if let Some(pending_idx) = self.pending_tap.take() {
                if pending_idx == idx && value == 0 {
                    // Bare 127 → 0: a button tap. Swallow both position jumps.
                    out.push(Action::FaderButtonTap { index: idx });
                    return;
                }
                // Pattern broken: flush the held 127 as real fader data,
                // then fall through to handle this value normally.
                out.push(Action::Fader {
                    index: idx,
                    value: 127,
                });
            }
            if value == 127 {
                self.pending_tap = Some(idx); // hold; might be a tap
                return;
            }
            out.push(Action::Fader { index: idx, value });
            return;
        }

        let controls_here = if self.m.control_any_channel {
            true
        } else {
            Self::on_channel(channel, self.m.control_channel)
        };
        if controls_here {
            if let Some(pos) = self.m.knobs.iter().position(|&c| c == control) {
                out.push(Action::Knob {
                    index: pos as u8 + 1,
                    value,
                });
                return;
            }
            if control >= self.m.fader_base_cc
                && control < self.m.fader_base_cc + self.m.fader_count
            {
                out.push(Action::Fader {
                    index: control - self.m.fader_base_cc + 1,
                    value,
                });
                return;
            }
            if control >= self.m.fader_button_base_cc
                && control < self.m.fader_button_base_cc + self.m.fader_button_count
            {
                out.push(Action::FaderButton {
                    index: control - self.m.fader_button_base_cc + 1,
                    pressed: value >= 64,
                });
                return;
            }
        }
        if Self::on_channel(channel, self.m.keys_channel) && control == self.m.mod_wheel_cc {
            out.push(Action::ModWheel(value));
        }
    }

    /// If (channel, cc) is the position CC of a shared-button fader → its index.
    fn shared_button_fader_for(&self, channel: u8, control: u8) -> Option<u8> {
        let controls_here = if self.m.control_any_channel {
            true
        } else {
            Self::on_channel(channel, self.m.control_channel)
        };
        if !controls_here {
            return None;
        }
        self.m.shared_button_faders.iter().find_map(|&idx| {
            let cc = self.m.fader_base_cc + idx - 1;
            (cc == control).then_some(idx)
        })
    }
}
