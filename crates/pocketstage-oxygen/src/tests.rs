//! Unit tests built from the T1 measured capture (see specs/device-maps/).

use super::*;
use pocketstage_midi::Event;

fn mapper() -> Mapper {
    Mapper::new(default_mapping())
}

// ---- faders: measured CC33..41 on ch13, physical order ----

#[test]
fn faders_map_by_measured_cc_numbers() {
    let m = mapper();
    for i in 0..9u8 {
        let ev = Event::ControlChange {
            channel: 12,
            control: 33 + i,
            value: 64,
        };
        assert_eq!(
            m.map(&ev),
            Some(Action::Fader {
                index: i + 1,
                value: 64
            })
        );
    }
}

// ---- knobs: measured non-contiguous CC order ----

#[test]
fn knobs_use_non_contiguous_measured_order() {
    let m = mapper();
    let physical_ccs = [44u8, 45, 46, 47, 62, 63, 75, 76];
    for (i, cc) in physical_ccs.iter().enumerate() {
        let ev = Event::ControlChange {
            channel: 12,
            control: *cc,
            value: 9,
        };
        assert_eq!(
            m.map(&ev),
            Some(Action::Knob {
                index: i as u8 + 1,
                value: 9
            })
        );
    }
}

// ---- fader buttons: CC49..56, pressed >= 64 ----

#[test]
fn fader_buttons_press_and_release() {
    let m = mapper();
    assert_eq!(
        m.map(&Event::ControlChange {
            channel: 12,
            control: 51,
            value: 127
        }),
        Some(Action::FaderButton {
            index: 3,
            pressed: true
        })
    );
    assert_eq!(
        m.map(&Event::ControlChange {
            channel: 12,
            control: 51,
            value: 0
        }),
        Some(Action::FaderButton {
            index: 3,
            pressed: false
        })
    );
}

// ---- pads on the keys channel (measured notes 36..43) ----

#[test]
fn pads_are_notes_36_to_43_on_keys_channel() {
    let m = mapper();
    for note in 36..=43u8 {
        let ev = Event::NoteOn {
            channel: 0,
            key: note,
            velocity: 100,
        };
        assert_eq!(
            m.map(&ev),
            Some(Action::Pad {
                index: note - 35,
                on: true,
                velocity: 100
            })
        );
    }
    let off = Event::NoteOff {
        channel: 0,
        key: 43,
        velocity: 0,
    };
    assert_eq!(
        m.map(&off),
        Some(Action::Pad {
            index: 8,
            on: false,
            velocity: 0
        })
    );
}

#[test]
fn keys_outside_pad_range_are_notes() {
    let m = mapper();
    // Middle C and the top keys captured live (measured 60, 84, 86, 93..95).
    for note in [44u8, 60, 84, 86, 93, 94, 95] {
        let ev = Event::NoteOn {
            channel: 0,
            key: note,
            velocity: 80,
        };
        assert_eq!(
            m.map(&ev),
            Some(Action::Key {
                note,
                on: true,
                velocity: 80
            })
        );
    }
}

// ---- wheel / pitch ----

#[test]
fn mod_wheel_on_keys_channel() {
    let m = mapper();
    assert_eq!(
        m.map(&Event::ControlChange {
            channel: 0,
            control: 1,
            value: 33
        }),
        Some(Action::ModWheel(33))
    );
    // CC1 on the CONTROL channel is not a knob (measured: it isn't) — ignored.
    assert_eq!(
        m.map(&Event::ControlChange {
            channel: 12,
            control: 1,
            value: 33
        }),
        None
    );
}

#[test]
fn pitch_bend_recenters() {
    let m = mapper();
    // Measured center was 0x2000.
    assert_eq!(
        m.map(&Event::PitchBend {
            channel: 0,
            value: 0x2000
        }),
        Some(Action::PitchBend(0))
    );
    assert_eq!(
        m.map(&Event::PitchBend {
            channel: 0,
            value: 0x1e80
        }), // measured wiggle value
        Some(Action::PitchBend(-384))
    );
    assert_eq!(
        m.map(&Event::PitchBend {
            channel: 0,
            value: 0x0
        }),
        Some(Action::PitchBend(-8192))
    );
}

// ---- transport = system real-time messages (measured!) ----

#[test]
fn transport_arrives_as_system_messages() {
    let m = mapper();
    assert_eq!(m.map(&Event::Start), Some(Action::Play));
    assert_eq!(m.map(&Event::Stop), Some(Action::Stop));
    assert_eq!(m.map(&Event::Reset), Some(Action::SystemReset));
}

// ---- noise discipline ----

#[test]
fn clock_and_sensing_are_swallowed() {
    let m = mapper();
    // The unit streams ~45 TimingClocks/s — they must never reach the engine.
    assert_eq!(m.map(&Event::TimingClock), None);
    assert_eq!(m.map(&Event::ActiveSensing), None);
    assert_eq!(m.map(&Event::Continue), None);
    assert_eq!(
        m.map(&Event::SystemExclusive {
            data: vec![0xF0, 0x7E, 0xF7]
        }),
        None
    );
    assert_eq!(
        m.map(&Event::ProgramChange {
            channel: 0,
            program: 5
        }),
        None
    );
}

#[test]
fn wrong_channel_control_is_ignored_in_strict_mode() {
    let m = mapper();
    // Same CC33 but on ch1 instead of ch13.
    assert_eq!(
        m.map(&Event::ControlChange {
            channel: 0,
            control: 33,
            value: 9
        }),
        None
    );
}

#[test]
fn any_channel_mode_accepts_controls_from_any_channel() {
    let mut map = default_mapping();
    map.control_any_channel = true;
    let m = Mapper::new(map);
    assert_eq!(
        m.map(&Event::ControlChange {
            channel: 4,
            control: 37,
            value: 50
        }),
        Some(Action::Fader {
            index: 5,
            value: 50
        })
    );
}

// ---- mapping validation & serde ----

#[test]
fn default_mapping_is_valid() {
    assert_eq!(default_mapping().validate(), Ok(()));
}

#[test]
fn mapping_json_round_trips() {
    let m = default_mapping();
    let json = m.to_json_string();
    let back = Mapping::from_json_str(&json).expect("round trip");
    assert_eq!(back, m);
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
