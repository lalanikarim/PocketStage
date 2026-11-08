//! Unit tests built from the T1 + probe A/B/C measured captures
//! (see specs/device-maps/oxygen-49-mkv.md).

use super::*;
use pocketstage_midi::Event;

fn map(m: &mut Mapper, ev: Event) -> Vec<Action> {
    let mut out = Vec::new();
    m.map(&ev, &mut out);
    out
}

fn std_mapper() -> Mapper {
    Mapper::new(default_mapping())
}

fn hui() -> Mapper {
    Mapper::new(hui_mapping())
}

// ---- faders: measured CC33..41 on ch13, physical order ----

#[test]
fn faders_map_by_measured_cc_numbers() {
    let mut m = std_mapper();
    for i in 0..9u8 {
        // Fader 9 is the shared-button one; use plain mid values here.
        let ev = Event::ControlChange {
            channel: 12,
            control: 33 + i,
            value: 64,
        };
        assert_eq!(
            map(&mut m, ev),
            vec![Action::Fader {
                index: i + 1,
                value: 64
            }]
        );
    }
}

// ---- knobs: measured non-contiguous CC order ----

#[test]
fn knobs_use_non_contiguous_measured_order() {
    let mut m = std_mapper();
    let physical_ccs = [44u8, 45, 46, 47, 62, 63, 75, 76];
    for (i, cc) in physical_ccs.iter().enumerate() {
        let ev = Event::ControlChange {
            channel: 12,
            control: *cc,
            value: 9,
        };
        assert_eq!(
            map(&mut m, ev),
            vec![Action::Knob {
                index: i as u8 + 1,
                value: 9
            }]
        );
    }
}

// ---- fader buttons 1..8: own CCs; #9 handled by tap detector below ----

#[test]
fn fader_buttons_press_and_release() {
    let mut m = std_mapper();
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 51,
                value: 127
            }
        ),
        vec![Action::FaderButton {
            index: 3,
            pressed: true
        }]
    );
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 56,
                value: 0
            }
        ),
        vec![Action::FaderButton {
            index: 8,
            pressed: false
        }]
    );
}

// ---- probe B: fader button 9 shares CC41 with fader 9's position ----

#[test]
fn bare_127_then_0_on_shared_cc_is_a_button_tap() {
    let mut m = std_mapper();
    // Measured press pattern at 66.1s: val127 → val0, nothing in between.
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 41,
                value: 127
            }
        ),
        vec![] // held pending — position jump swallowed for now
    );
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 41,
                value: 0
            }
        ),
        vec![Action::FaderButtonTap { index: 9 }] // both jumps swallowed
    );
}

#[test]
fn sweeping_fader_through_the_top_is_not_a_tap() {
    let mut m = std_mapper();
    let seq = [100u8, 127, 126, 40];
    let mut got = Vec::new();
    for v in seq {
        let out = map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 41,
                value: v,
            },
        );
        got.extend(out);
    }
    assert_eq!(
        got,
        vec![
            Action::Fader {
                index: 9,
                value: 100
            },
            Action::Fader {
                index: 9,
                value: 127
            }, // flushed when 126 broke the pattern
            Action::Fader {
                index: 9,
                value: 126
            },
            Action::Fader {
                index: 9,
                value: 40
            },
        ]
    );
}

#[test]
fn held_top_flushes_when_other_events_arrive() {
    let mut m = std_mapper();
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 41,
                value: 127
            }
        ),
        vec![]
    );
    // Fader stays at top; user plays a note — held position must surface.
    let out = map(
        &mut m,
        Event::NoteOn {
            channel: 0,
            key: 60,
            velocity: 70,
        },
    );
    assert_eq!(
        out,
        vec![
            Action::Fader {
                index: 9,
                value: 127
            }, // flush first — ordering honesty
            Action::Key {
                note: 60,
                on: true,
                velocity: 70
            },
        ]
    );
}

// ---- pads (probe C): banks give notes 36..51 = logical pads 1..16 ----

#[test]
fn pad_bank_one_is_notes_36_to_43() {
    let mut m = std_mapper();
    for note in 36..=43u8 {
        let ev = Event::NoteOn {
            channel: 0,
            key: note,
            velocity: 100,
        };
        assert_eq!(
            map(&mut m, ev),
            vec![Action::Pad {
                index: note - 35,
                on: true,
                velocity: 100
            }]
        );
    }
}

#[test]
fn pad_bank_two_is_notes_44_to_51() {
    let mut m = std_mapper();
    for note in 44..=51u8 {
        let ev = Event::NoteOn {
            channel: 0,
            key: note,
            velocity: 90,
        };
        assert_eq!(
            map(&mut m, ev),
            vec![Action::Pad {
                index: note - 35,
                on: true,
                velocity: 90
            }]
        );
    }
    let off = Event::NoteOff {
        channel: 0,
        key: 51,
        velocity: 0,
    };
    assert_eq!(
        map(&mut m, off),
        vec![Action::Pad {
            index: 16,
            on: false,
            velocity: 0
        }]
    );
}

#[test]
fn keys_outside_pad_window_are_notes() {
    let mut m = std_mapper();
    for note in [35u8, 52, 60, 84, 95] {
        let ev = Event::NoteOn {
            channel: 0,
            key: note,
            velocity: 80,
        };
        assert_eq!(
            map(&mut m, ev),
            vec![Action::Key {
                note,
                on: true,
                velocity: 80
            }]
        );
    }
}

// ---- wheel / pitch ----

#[test]
fn mod_wheel_on_keys_channel() {
    let mut m = std_mapper();
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 0,
                control: 1,
                value: 33
            }
        ),
        vec![Action::ModWheel(33)]
    );
    // CC1 on the CONTROL channel is not a knob — ignored.
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 12,
                control: 1,
                value: 33
            }
        ),
        vec![]
    );
}

#[test]
fn pitch_bend_recenters() {
    let mut m = std_mapper();
    assert_eq!(
        map(
            &mut m,
            Event::PitchBend {
                channel: 0,
                value: 0x2000
            }
        ),
        vec![Action::PitchBend(0)]
    );
    assert_eq!(
        map(
            &mut m,
            Event::PitchBend {
                channel: 0,
                value: 0x1e80
            }
        ), // measured wiggle
        vec![Action::PitchBend(-384)]
    );
    assert_eq!(
        map(
            &mut m,
            Event::PitchBend {
                channel: 0,
                value: 0x0
            }
        ),
        vec![Action::PitchBend(-8192)]
    );
}

// ---- transport (probe A2): system messages + HUI-port notes ----

#[test]
fn system_messages_on_music_port() {
    let mut m = std_mapper();
    assert_eq!(map(&mut m, Event::Start), vec![Action::Play]);
    assert_eq!(map(&mut m, Event::Stop), vec![Action::Stop]);
    assert_eq!(map(&mut m, Event::Reset), vec![Action::SystemReset]);
}

#[test]
fn hui_port_transport_notes() {
    let mut m = hui();
    // Measured notes: loop 86, stop 93, play 94, record 95 — vel 127.
    assert_eq!(
        map(
            &mut m,
            Event::NoteOn {
                channel: 0,
                key: 86,
                velocity: 127
            }
        ),
        vec![Action::LoopToggle]
    );
    assert_eq!(
        map(
            &mut m,
            Event::NoteOn {
                channel: 0,
                key: 93,
                velocity: 127
            }
        ),
        vec![Action::Stop]
    );
    assert_eq!(
        map(
            &mut m,
            Event::NoteOn {
                channel: 0,
                key: 94,
                velocity: 127
            }
        ),
        vec![Action::Play]
    );
    assert_eq!(
        map(
            &mut m,
            Event::NoteOn {
                channel: 0,
                key: 95,
                velocity: 127
            }
        ),
        vec![Action::Record] // the ONLY way to tell record from play!
    );
    // Releases are silent.
    assert_eq!(
        map(
            &mut m,
            Event::NoteOff {
                channel: 0,
                key: 95,
                velocity: 0
            }
        ),
        vec![]
    );
}

#[test]
fn music_port_top_octave_stays_musical() {
    // Without the HUI mapping, notes 86/93/94/95 are ordinary keys.
    let mut m = std_mapper();
    for key in [86u8, 93, 94, 95] {
        assert_eq!(
            map(
                &mut m,
                Event::NoteOn {
                    channel: 0,
                    key,
                    velocity: 77
                }
            ),
            vec![Action::Key {
                note: key,
                on: true,
                velocity: 77
            }]
        );
    }
}

// ---- noise discipline ----

#[test]
fn clock_and_sensing_are_swallowed() {
    let mut m = std_mapper();
    assert_eq!(map(&mut m, Event::TimingClock), vec![]);
    assert_eq!(map(&mut m, Event::ActiveSensing), vec![]);
    assert_eq!(map(&mut m, Event::Continue), vec![]);
    assert_eq!(
        map(
            &mut m,
            Event::SystemExclusive {
                data: vec![0xF0, 0x7E, 0xF7]
            }
        ),
        vec![]
    );
    assert_eq!(
        map(
            &mut m,
            Event::ProgramChange {
                channel: 0,
                program: 5
            }
        ),
        vec![]
    );
}

#[test]
fn wrong_channel_control_is_ignored_in_strict_mode() {
    let mut m = std_mapper();
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 0,
                control: 33,
                value: 9
            }
        ),
        vec![]
    );
}

#[test]
fn any_channel_mode_accepts_controls_from_any_channel() {
    let mut map_cfg = default_mapping();
    map_cfg.control_any_channel = true;
    let mut m = Mapper::new(map_cfg);
    assert_eq!(
        map(
            &mut m,
            Event::ControlChange {
                channel: 4,
                control: 37,
                value: 50
            }
        ),
        vec![Action::Fader {
            index: 5,
            value: 50
        }]
    );
}

// ---- mapping validation & serde ----

#[test]
fn default_mapping_is_valid() {
    assert_eq!(default_mapping().validate(), Ok(()));
    assert_eq!(hui_mapping().validate(), Ok(()));
}

#[test]
fn mapping_json_round_trips() {
    let m = default_mapping();
    let json = m.to_json_string();
    let back = Mapping::from_json_str(&json).expect("round trip");
    assert_eq!(back, m);
}

#[test]
fn old_json_without_new_fields_still_loads() {
    // serde defaults must cover shared_button_faders & transport_notes.
    let json = r#"{
        "control_channel": 13, "keys_channel": 1, "pad_note_base": 36, "pad_count": 16,
        "mod_wheel_cc": 1, "knobs": [44,45,46,47,62,63,75,76],
        "fader_base_cc": 33, "fader_count": 9,
        "fader_button_base_cc": 49, "fader_button_count": 8
    }"#;
    let m = Mapping::from_json_str(json).expect("legacy json");
    assert_eq!(m, default_mapping());
}

#[test]
fn unknown_json_fields_are_rejected() {
    let bad = r#"{ "control_channel": 13, "typo_field": 1 }"#;
    assert!(Mapping::from_json_str(bad).is_err());
}

#[test]
fn duplicate_cc_claims_are_rejected() {
    let mut m = default_mapping();
    m.fader_base_cc = 44; // collides with knob 1 (measured CC44)
    assert!(
        m.validate().is_err(),
        "ambiguous CC claim must fail validation"
    );
}

#[test]
fn channel_out_of_range_rejected() {
    let mut m = default_mapping();
    m.control_channel = 17;
    assert!(m.validate().is_err());
}

#[test]
fn shared_button_fader_index_validated() {
    let mut m = default_mapping();
    m.shared_button_faders = vec![10]; // only 9 faders exist
    assert!(m.validate().is_err());
}
