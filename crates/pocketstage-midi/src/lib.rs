//! PocketStage MIDI I/O layer (backlog #2).
//!
//! Two pieces so far:
//! * **Port discovery** ([`list_input_ports`]) — enumerates ALSA rawmidi
//!   capture subdevices with human names, no config required.
//! * **A streaming byte parser** ([`Parser`]) — turns raw USB-MIDI byte
//!   streams into [`Event`]s (running status handled). Pure and testable;
//!   hardware only ever feeds it bytes.
//!
//! Later (this crate's remaining scope): hotplug monitoring, the event pump
//! that bridges rawmidi -> engine mpsc boundary, and mapping hooks for
//! `pocketstage-oxygen`.

use alsa::{Direction, card};

/// One discoverable rawmidi **input** subdevice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputPort {
    pub card_index: i32,
    pub device: i32,
    pub subdevice: i32,
    /// ALSA short card name, e.g. `"MKV"`.
    pub card_name: String,
    /// Subdevice name, e.g. `"Oxygen 49 MKV USB MIDI"`.
    pub subdevice_name: String,
}

impl InputPort {
    /// Device string for `Rawmidi::new`, e.g. `"hw:3,0,0"`.
    pub fn device_string(&self) -> String {
        format!("hw:{},{},{}", self.card_index, self.device, self.subdevice)
    }
}

impl std::fmt::Display for InputPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<12} {} — {}",
            self.device_string(),
            self.card_name,
            self.subdevice_name
        )
    }
}

/// Enumerate all rawmidi capture subdevices on the system.
///
/// Cards that vanish mid-scan (hotplug race) are skipped, not fatal.
pub fn list_input_ports() -> Vec<InputPort> {
    let mut ports = Vec::new();
    for card in card::Iter::new().flatten() {
        let Ok(ctl) = alsa::Ctl::from_card(&card, false) else {
            continue; // busy/broken card — not our problem for listing
        };
        let card_name = card.get_name().unwrap_or_else(|_| "?".into());
        for info in alsa::rawmidi::Iter::new(&ctl).flatten() {
            if info.get_stream() != Direction::Capture {
                continue;
            }
            ports.push(InputPort {
                card_index: card.get_index(),
                device: info.get_device(),
                subdevice: info.get_subdevice(),
                card_name: card_name.clone(),
                subdevice_name: info.get_subdevice_name().unwrap_or_else(|_| "?".into()),
            });
        }
    }
    ports
}

// ------------------------------------------------------------------ events

/// A decoded MIDI message (channel voice + common system messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    NoteOff {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    NoteOn {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    PolyPressure {
        channel: u8,
        key: u8,
        value: u8,
    },
    ControlChange {
        channel: u8,
        control: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    ChannelPressure {
        channel: u8,
        value: u8,
    },
    PitchBend {
        channel: u8,
        value: u16,
    }, // 14-bit, centered at 0x2000
    SystemExclusive {
        data: Vec<u8>,
    }, // includes 0xF0 and 0xF7 framing
    TimingClock,
    Start,
    Continue,
    Stop,
    ActiveSensing,
    Reset,
    /// Bytes the parser could not interpret (e.g. stray status byte).
    Unknown {
        status: u8,
        data: Vec<u8>,
    },
}

/// Streaming MIDI 1.0 byte-stream decoder with running-status support.
///
/// Not audio-path optimized (this is discovery/probing); the production pump
/// will decode straight into engine events. Real-time messages *interleaved
/// inside* a running voice message are not re-ordered (probes never hit that;
/// noted for the implementer of the real pump).
#[derive(Default)]
pub struct Parser {
    /// Running status (high bit set), or 0 when none.
    running: u8,
    /// Status byte of the message currently being collected (0 = idle).
    pending_status: u8,
    data: Vec<u8>,
    in_sysex: bool,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of data bytes (excluding status) the message needs.
    fn data_len(status: u8) -> usize {
        match status & 0xF0 {
            0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 2,
            0xC0 | 0xD0 => 1,
            // System common: 0xF1 SongSel, 0xF3 SongPos(2), 0xF2; 0xF4/0xF5 standalone
            0xF0 => match status {
                0xF1 | 0xF3 => 1,
                0xF2 => 2,
                _ => 0,
            },
            _ => 0, // realtime: single byte
        }
    }

    /// Feed raw bytes; decoded events are appended to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Event>) {
        for &b in bytes {
            // SysEx collection mode: swallow everything until EOX.
            if self.in_sysex {
                self.data.push(b);
                if b == 0xF7 {
                    out.push(Event::SystemExclusive {
                        data: std::mem::take(&mut self.data),
                    });
                    self.in_sysex = false;
                }
                continue;
            }

            // Realtime bytes (0xF8..=0xFF) are transparent: never flush or
            // disturb a pending message.
            if b >= 0xF8 {
                out.push(realtime_event(b));
                continue;
            }

            // Any other status byte (>= 0x80) starts/aborts the current message.
            if b & 0x80 != 0 {
                // Flush an incomplete pending message as Unknown before restarting.
                self.flush_unknown(out);

                match b {
                    0xF0 => {
                        self.in_sysex = true;
                        self.data.push(b);
                        continue;
                    }
                    0xF7 => {
                        // Stray EOX — report and ignore.
                        out.push(Event::Unknown {
                            status: b,
                            data: Vec::new(),
                        });
                        continue;
                    }
                    status => {
                        let needs = Self::data_len(status);
                        if needs == 0 {
                            out.push(Event::Unknown {
                                status,
                                data: Vec::new(),
                            });
                            continue;
                        }
                        self.pending_status = status;
                        // Keep running status updated (voice messages only).
                        if status & 0xF0 < 0xF0 {
                            self.running = status;
                        }
                        continue;
                    }
                }
            }

            // Data byte: continues the pending message; if none is open, restart
            // from running status (MIDI's implicit repeat). Garbage otherwise.
            if self.pending_status == 0 {
                if self.running == 0 {
                    out.push(Event::Unknown {
                        status: 0,
                        data: vec![b],
                    });
                    continue;
                }
                self.pending_status = self.running;
            }
            self.data.push(b);
            let needs = Self::data_len(self.pending_status);
            if self.data.len() == needs {
                let d = std::mem::take(&mut self.data);
                if self.pending_status < 0xF0 {
                    out.push(voice_event(self.pending_status, &d));
                } else {
                    // System common (Song Select/Position): report raw for probing.
                    out.push(Event::Unknown {
                        status: self.pending_status,
                        data: d,
                    });
                }
                self.pending_status = 0;
            }
        }
    }

    fn flush_unknown(&mut self, out: &mut Vec<Event>) {
        if self.pending_status != 0 {
            let d = std::mem::take(&mut self.data);
            out.push(Event::Unknown {
                status: self.pending_status,
                data: d,
            });
            self.pending_status = 0;
        }
    }
}

fn realtime_event(status: u8) -> Event {
    match status {
        0xF8 => Event::TimingClock,
        0xFA => Event::Start,
        0xFB => Event::Continue,
        0xFC => Event::Stop,
        0xFE => Event::ActiveSensing,
        _ => Event::Reset, // 0xFF
    }
}

fn voice_event(status: u8, d: &[u8]) -> Event {
    let ch = status & 0x0F;
    match status & 0xF0 {
        0x80 => Event::NoteOff {
            channel: ch,
            key: d[0],
            velocity: d[1],
        },
        0x90 => Event::NoteOn {
            channel: ch,
            key: d[0],
            velocity: d[1],
        },
        0xA0 => Event::PolyPressure {
            channel: ch,
            key: d[0],
            value: d[1],
        },
        0xB0 => Event::ControlChange {
            channel: ch,
            control: d[0],
            value: d[1],
        },
        0xC0 => Event::ProgramChange {
            channel: ch,
            program: d[0],
        },
        0xD0 => Event::ChannelPressure {
            channel: ch,
            value: d[0],
        },
        _ => Event::PitchBend {
            channel: ch,
            value: (d[0] as u16) | ((d[1] as u16) << 7),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(bytes, &mut out);
        out
    }

    #[test]
    fn note_on_off() {
        assert_eq!(
            parse(&[0x90, 60, 100, 0x80, 60, 0]),
            vec![
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100
                },
                Event::NoteOff {
                    channel: 0,
                    key: 60,
                    velocity: 0
                },
            ]
        );
    }

    #[test]
    fn running_status_reuses_last_voice_message() {
        // One status byte, four data bytes = two NoteOns.
        assert_eq!(
            parse(&[0x92, 60, 100, 62, 90]),
            vec![
                Event::NoteOn {
                    channel: 2,
                    key: 60,
                    velocity: 100
                },
                Event::NoteOn {
                    channel: 2,
                    key: 62,
                    velocity: 90
                },
            ]
        );
    }

    #[test]
    fn program_change_is_one_data_byte() {
        assert_eq!(
            parse(&[0xC1, 4]),
            vec![Event::ProgramChange {
                channel: 1,
                program: 4
            }]
        );
    }

    #[test]
    fn pitch_bend_is_14_bit() {
        // LSB then MSB; 0x2000 = center.
        assert_eq!(
            parse(&[0xE0, 0x00, 0x40]),
            vec![Event::PitchBend {
                channel: 0,
                value: 0x2000
            }]
        );
    }

    #[test]
    fn sysex_collected_until_eox_across_feeds() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(&[0xF0, 0x00, 0x51], &mut out);
        assert!(out.is_empty()); // M-Audio vendor id starts 00 51...
        p.feed(&[0x3F, 0x01, 0xF7], &mut out);
        assert_eq!(
            out,
            vec![Event::SystemExclusive {
                data: vec![0xF0, 0x00, 0x51, 0x3F, 0x01, 0xF7]
            }]
        );
    }

    #[test]
    fn realtime_does_not_disturb_running_status() {
        // NoteOn running; clock bytes in between are dropped out-of-band.
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(&[0x90, 60], &mut out); // half a message
        p.feed(&[0xF8, 0xF8], &mut out); // clocks
        p.feed(&[100], &mut out); // completes the NoteOn
        assert_eq!(
            out,
            vec![
                Event::TimingClock,
                Event::TimingClock,
                Event::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 100
                },
            ]
        );
    }

    #[test]
    fn incomplete_message_reported_unknown_when_next_status_arrives() {
        let events = parse(&[0x90, 60, 0xB0, 7, 64]);
        assert_eq!(
            events,
            vec![
                Event::Unknown {
                    status: 0x90,
                    data: vec![60]
                },
                Event::ControlChange {
                    channel: 0,
                    control: 7,
                    value: 64
                },
            ]
        );
    }

    #[test]
    fn system_common_is_not_mistaken_for_pitch_bend() {
        assert_eq!(
            parse(&[0xF3, 5]), // Song Select, song 5
            vec![Event::Unknown {
                status: 0xF3,
                data: vec![5]
            }]
        );
    }

    #[test]
    fn device_string_format() {
        let p = InputPort {
            card_index: 3,
            device: 0,
            subdevice: 1,
            card_name: "MKV".into(),
            subdevice_name: "Oxygen 49 MKV Mackie".into(),
        };
        assert_eq!(p.device_string(), "hw:3,0,1");
    }

    #[test]
    fn enumeration_does_not_panic_on_any_machine() {
        // Hardware-dependent by nature: must simply not crash with zero or N cards.
        let _ = list_input_ports();
    }
}
